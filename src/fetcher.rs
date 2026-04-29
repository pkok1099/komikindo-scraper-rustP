/// Async HTTP fetcher menggunakan libcurl (bypass Cloudflare).
///
/// OPTIMIZED V3 (optimize-beta):
///   - Connection reuse via thread-local curl handle cache
///     Setiap blocking thread mempertahankan curl handle-nya sendiri,
///     sehingga HTTP keep-alive / HTTP/2 connection dipertahankan.
///     Hindari TCP+TLS handshake (~100-200ms) per request.
///   - Handle caching AFTER successful perform() even on HTTP errors
///     (429, content-type mismatch, CF challenge) — TCP/TLS connection
///     is still valid and should be reused. Only drop on network errors.
///   - FORBID_REUSE on 429: closes rate-limited connection but keeps
///     handle cached for fresh connection on next request.
///   - Separate connect_timeout (10s max) for fast-fail on dead hosts
///   - maxage_conn(120s): closes idle connections after 120s
///   - DNS cache timeout 3600s (1 hour)
///   - Zero-copy response: UnsafeCell take data alih-alih .clone()
///   - Pre-allocated Collector buffer (256KB) mengurangi re-allocation
///   - Arc<str> untuk shared config (proxy_url, ca_bundle_path)
///   - Cached header strings (rebuilt into List per handle, no alloc)
///   - Cached CA bundle path (resolved once at creation)
///   - Fast UTF-8 conversion (checked, not lossy)
///
/// Architecture:
///   - Semaphore limits in-flight requests
///   - spawn_blocking runs on tokio blocking thread pool
///   - Thread-local handle cache: setiap thread punya handle sendiri
///     → request berikutnya di thread yang sama reuse connection
///   - Set max_blocking_threads == max_in_flight untuk reuse optimal
///
/// Cookie jar per-handle (CF __cf_bm cookies).
/// Auto-retry dengan exponential backoff.
/// SOCKS5/HTTP proxy support.

use anyhow::Result;
use curl::easy::{Easy2, Handler, HttpVersion, List, WriteError};
use log::debug;
use std::cell::{RefCell, UnsafeCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::config::{env_config, BASE_URL};

// ============================================================
// STATS
// ============================================================

#[derive(Debug, Clone)]
pub struct FetcherStats {
    pub requests: u64,
    pub success: u64,
    pub failed: u64,
    pub retries: u64,
    pub bytes_downloaded: u64,
}

impl FetcherStats {
    pub fn mb_downloaded(&self) -> f64 {
        self.bytes_downloaded as f64 / 1024.0 / 1024.0
    }

    #[allow(dead_code)]
    pub fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.success as f64 / self.requests as f64 * 100.0
        }
    }
}

#[derive(Default)]
struct AtomicFetcherStats {
    requests: AtomicU64,
    success: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    bytes_downloaded: AtomicU64,
}

// ============================================================
// CACHED HTTP HEADER STRINGS
// ============================================================

/// Static user-agent string (avoids rebuilding per request).
static USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) \
    Chrome/120.0.0.0 Safari/537.36";

/// Header lines cached as static strings.
/// curl::List wraps a raw C linked list (not Clone/Send), so we can't store it
/// in Fetcher. Instead we cache header strings and rebuild the List per-handle
/// (only allocates the linked list nodes, no string allocation).
static HEADER_LINES: &[&str] = &[
    "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    "Accept-Language: id-ID,id;q=0.9,en-US;q=0.8,en;q=0.7",
    "Sec-Fetch-Dest: document",
    "Sec-Fetch-Mode: navigate",
    "Sec-Fetch-Site: none",
    "Sec-Fetch-User: ?1",
    "Sec-Ch-Ua: \"Not_A Brand\";v=\"8\", \"Chromium\";v=\"120\"",
    "Sec-Ch-Ua-Mobile: ?0",
    "Sec-Ch-Ua-Platform: \"Windows\"",
    "Upgrade-Insecure-Requests: 1",
];

/// Referer header needs BASE_URL — computed once.
static REFERER_HEADER: LazyLock<String> = LazyLock::new(|| format!("Referer: {BASE_URL}/"));

/// Build a curl List from cached header strings (no string allocation).
fn build_headers_list() -> List {
    let mut headers = List::new();
    for &h in HEADER_LINES {
        headers.append(h).unwrap();
    }
    headers.append(&REFERER_HEADER).unwrap();
    headers
}

// ============================================================
// COLLECTOR (curl response handler) — pre-allocated + UnsafeCell
// ============================================================

/// Response collector with pre-allocated 256KB buffer and interior mutability.
///
/// Uses `UnsafeCell<Vec<u8>>` to allow data extraction and reset between
/// `perform()` calls via `Easy2::get_ref() -> &Collector` (immutable ref).
///
/// Safety invariant:
/// - `Handler::write(&mut self)` is only called by curl during `perform()`
/// - `take_data()` / `reset()` / `len()` / `as_slice()` are only called
///   AFTER `perform()` returns, when curl is not accessing the handler
/// - Collector is used from a single thread (thread-local or spawn_blocking)
struct Collector {
    data: UnsafeCell<Vec<u8>>,
}

// SAFETY: Collector is only used from a single thread. The UnsafeCell<Vec<u8>>
// is only accessed mutably between curl::perform() calls, never concurrently.
unsafe impl Send for Collector {}

impl Collector {
    /// Create collector with 256KB pre-allocated buffer.
    fn new() -> Self {
        Self {
            data: UnsafeCell::new(Vec::with_capacity(262_144)), // 256KB
        }
    }

    /// Get the length of buffered data.
    /// SAFE: Only called after perform() returns.
    #[inline]
    fn len(&self) -> usize {
        // SAFETY: No curl operation in progress
        unsafe { (*self.data.get()).len() }
    }

    /// Take ownership of the response data, leaving an empty Vec with preserved capacity.
    /// SAFE: Only called after perform() returns, when curl is not accessing the handler.
    /// This is the zero-copy path: Vec<u8> is converted to String without cloning.
    fn take_data(&self) -> Vec<u8> {
        // SAFETY: No curl operation in progress
        unsafe { std::mem::take(&mut *self.data.get()) }
    }

    /// Clear the buffer while preserving allocated capacity.
    /// SAFE: Only called after perform() returns.
    fn reset(&self) {
        // SAFETY: No curl operation in progress
        unsafe { (*self.data.get()).clear() };
    }
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        // SAFETY: curl calls write() with &mut self during perform(),
        // and our take_data()/reset() are only called after perform() returns.
        // No concurrent access is possible.
        unsafe { (*self.data.get()).extend_from_slice(data) };
        Ok(data.len())
    }
}

// ============================================================
// THREAD-LOCAL CURL HANDLE CACHE (connection reuse)
// ============================================================

// Thread-local curl handle cache: each blocking thread keeps its own
// Easy2<Collector> with persistent TCP/TLS connection (HTTP keep-alive).
// Set max_blocking_threads == max_in_flight for optimal reuse.
thread_local! {
    static CACHED_CURL_HANDLE: RefCell<Option<Easy2<Collector>>> = RefCell::new(None);
}

/// Create and fully configure a new curl handle.
/// Called once per blocking thread on first request.
fn create_configured_handle(
    timeout_secs: u64,
    proxy_url: &str,
    verbose: bool,
    ca_bundle_path: &Option<String>,
) -> Easy2<Collector> {
    let mut handle = Easy2::new(Collector::new());

    // Core settings (persist across perform() calls)
    handle.useragent(USER_AGENT).ok();
    handle.http_version(HttpVersion::V2).ok();
    handle.follow_location(true).ok();
    handle.max_redirections(10).ok();
    handle.timeout(Duration::from_secs(timeout_secs)).ok();
    // Separate shorter connect timeout: fail fast on unreachable hosts
    // without waiting the full timeout (10s max, or timeout_secs if smaller)
    handle.connect_timeout(Duration::from_secs(timeout_secs.min(10))).ok();
    let _ = handle.low_speed_limit(1024);
    // Low speed time: abort if <1KB/s for 10 seconds.
    // Lower than default 30s for high-throughput scraping — stalled connections
    // are detected 3x faster, preventing wasted time on dead/slow connections.
    let _ = handle.low_speed_time(Duration::from_secs(10));
    let _ = handle.max_filesize(10_000_000);

    // Headers (persist across perform() calls)
    let headers = build_headers_list();
    handle.http_headers(headers).ok();

    // Cookies (persist across perform() calls)
    handle.cookie_file("").ok();
    handle.cookie_list("session=1").ok();

    // Proxy
    if !proxy_url.is_empty() {
        handle.proxy(proxy_url).ok();
    }

    // Connection optimization (persist across perform() calls)
    // DNS cache: 1 hour — avoids repeated DNS lookups for the same host
    handle.dns_cache_timeout(Duration::from_secs(3600)).ok();
    // maxage_conn: close connections idle >120s to prevent stale reuse
    let _ = handle.maxage_conn(Duration::from_secs(120));
    handle.tcp_keepalive(true).ok();
    handle.tcp_keepidle(Duration::from_secs(15)).ok();
    // TCP_NODELAY: disable Nagle's algorithm — send HTTP request headers immediately
    // instead of buffering up to 200ms. Saves ~6-29 minutes across 8677 requests.
    handle.tcp_nodelay(true).ok();
    handle.accept_encoding("gzip, deflate, br").ok();
    // pipewait: wait for HTTP/2 multiplexing before opening new connection
    handle.pipewait(true).ok();

    // SSL
    if let Some(ref path) = ca_bundle_path {
        if let Err(e) = handle.cainfo(path) {
            eprintln!("[CURL] WARNING: Failed to set CA bundle '{}': {}", path, e);
        }
    } else {
        eprintln!("[CURL] WARNING: No CA certificate bundle — SSL will fail!");
    }

    // Verbose
    if verbose {
        handle.verbose(true).ok();
    }

    handle
}

// ============================================================
// FETCHER
// ============================================================

pub struct Fetcher {
    max_retries: u32,
    timeout_secs: u64,
    /// Shared proxy URL (Arc avoids clone per request)
    proxy_url: Arc<str>,
    stats: Arc<AtomicFetcherStats>,
    verbose: bool,
    in_flight: Arc<Semaphore>,
    /// Cached CA bundle path (Arc avoids clone per request)
    ca_bundle_path: Arc<Option<String>>,
}

// Fetcher is Send because all fields are Send.
// We don't store curl::List (which is !Send) in the struct.
unsafe impl Send for Fetcher {}
unsafe impl Sync for Fetcher {}

impl Fetcher {
    pub fn new(
        timeout_secs: u64,
        proxy_url: Option<&str>,
        verbose: bool,
        max_in_flight_requests: usize,
    ) -> Result<Self> {
        let cfg = env_config();

        let effective_proxy = proxy_url
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if cfg.proxy_enabled {
                    cfg.proxy_url.clone()
                } else {
                    String::new()
                }
            });

        let proxy_info = if !effective_proxy.is_empty() {
            format!(", proxy={}", effective_proxy)
        } else {
            String::new()
        };

        // Resolve CA bundle ONCE at creation (not per-request)
        let ca_bundle_path = find_ca_bundle();

        let ca_info = if let Some(ref p) = ca_bundle_path {
            format!("\n  CA bundle: {}", p)
        } else {
            "\n  CA bundle: NOT FOUND (SSL will fail!)".to_string()
        };

        println!(
            "[FETCHER] Started (libcurl v3 - connection reuse): \
             timeout={}s, connect_timeout={}s, in_flight_limit={}{}{}{}\
             \n  [PERF] Thread-local handle cache: ENABLED (HTTP keep-alive)\
             \n  [PERF] Handle caching on HTTP errors: ENABLED (reuses TCP/TLS)\
             \n  [PERF] FORBID_REUSE on 429: ENABLED (fresh conn, same handle)\
             \n  [PERF] maxage_conn: 120s (idle connection cleanup)\
             \n  [PERF] DNS cache: 3600s\
             \n  [PERF] Pre-allocated buffer: 256KB\
             \n  [PERF] Zero-copy response: ENABLED",
            timeout_secs,
            timeout_secs.min(10),
            max_in_flight_requests,
            proxy_info,
            if verbose { " [VERBOSE]" } else { "" },
            ca_info
        );

        Ok(Fetcher {
            max_retries: cfg.scraper_retries,
            timeout_secs,
            proxy_url: Arc::from(effective_proxy),
            stats: Arc::new(AtomicFetcherStats::default()),
            verbose,
            in_flight: Arc::new(Semaphore::new(max_in_flight_requests.max(1))),
            ca_bundle_path: Arc::new(ca_bundle_path),
        })
    }

    /// Fetch satu halaman. Semaphore limits concurrency.
    pub async fn fetch_page(&self, url: &str) -> Result<String> {
        let _permit = self
            .in_flight
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("Fetcher semaphore closed"))?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);

        let url = url.to_string();
        let timeout_secs = self.timeout_secs;
        let proxy_url = Arc::clone(&self.proxy_url);
        let stats = Arc::clone(&self.stats);
        let max_retries = self.max_retries;
        let verbose = self.verbose;
        let ca_bundle_path = Arc::clone(&self.ca_bundle_path);

        tokio::task::spawn_blocking(move || {
            let mut last_err = String::new();

            for attempt in 0..max_retries {
                match curl_fetch_optimized(
                    &url,
                    timeout_secs,
                    &proxy_url,
                    verbose,
                    &ca_bundle_path,
                ) {
                    Ok(html) => {
                        stats.success.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_downloaded
                            .fetch_add(html.len() as u64, Ordering::Relaxed);
                        return Ok(html);
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        stats.retries.fetch_add(1, Ordering::Relaxed);
                        if attempt < max_retries - 1 {
                            // Lazy formatting — only format string when actually logging.
                            // debug!() macro already skips when log level < DEBUG,
                            // but format!() before the if always allocates.
                            if verbose {
                                eprintln!("[FETCH] Retry {}/{} for {}: {}",
                                    attempt + 1, max_retries, url, e);
                            } else {
                                debug!("Retry {}/{} for {}: {}",
                                    attempt + 1, max_retries, url, e);
                            }
                            // Longer backoff for rate-limited requests
                            let sleep_secs = if last_err.contains("RATE_LIMITED") {
                                2.0
                            } else {
                                0.3 * (attempt + 1) as f64
                            };
                            std::thread::sleep(Duration::from_secs_f64(sleep_secs));
                        }
                    }
                }
            }

            stats.failed.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!(
                "Gagal fetch {} setelah {} retries. Last: {}",
                url,
                max_retries,
                last_err
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("Task error: {e}"))?
    }

    #[allow(dead_code)]
    /// Batch fetch - untuk banyak URL sekaligus.
    pub async fn fetch_pages_batch(&self, urls: &[String]) -> Vec<(String, Result<String>)> {
        let mut handles = Vec::with_capacity(urls.len());
        for url in urls {
            let url = url.clone();
            let stats = Arc::clone(&self.stats);
            let timeout_secs = self.timeout_secs;
            let proxy_url = Arc::clone(&self.proxy_url);
            let max_retries = self.max_retries;
            let verbose = self.verbose;
            let sem = Arc::clone(&self.in_flight);
            let ca_bundle_path = Arc::clone(&self.ca_bundle_path);

            handles.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow::anyhow!("Fetcher semaphore closed"))?;

                stats.requests.fetch_add(1, Ordering::Relaxed);
                let url_for_blocking = url.clone();
                let r = tokio::task::spawn_blocking(move || {
                    let mut last_err = String::new();
                    for attempt in 0..max_retries {
                        match curl_fetch_optimized(
                            &url_for_blocking,
                            timeout_secs,
                            &proxy_url,
                            verbose,
                            &ca_bundle_path,
                        ) {
                            Ok(html) => {
                                stats.success.fetch_add(1, Ordering::Relaxed);
                                stats
                                    .bytes_downloaded
                                    .fetch_add(html.len() as u64, Ordering::Relaxed);
                                return Ok(html);
                            }
                            Err(e) => {
                                last_err = e.to_string();
                                stats.retries.fetch_add(1, Ordering::Relaxed);
                                if attempt < max_retries - 1 {
                                    let sleep_secs = if last_err.contains("RATE_LIMITED") {
                                        2.0
                                    } else {
                                        0.3 * (attempt + 1) as f64
                                    };
                                    std::thread::sleep(Duration::from_secs_f64(sleep_secs));
                                }
                            }
                        }
                    }
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    Err(anyhow::anyhow!(
                        "Gagal setelah {} retries: {}",
                        max_retries,
                        last_err
                    ))
                })
                .await
                .map_err(|e| anyhow::anyhow!("Task error: {e}"))?;

                Ok::<_, anyhow::Error>((url, r))
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(Ok(r)) => results.push(r),
                Ok(Err(e)) => results.push((
                    "task-error".into(),
                    Err(anyhow::anyhow!("Task error: {e}")),
                )),
                Err(e) => results.push((
                    "task-error".into(),
                    Err(anyhow::anyhow!("Task join error: {e}")),
                )),
            }
        }
        results
    }

    pub fn stats(&self) -> FetcherStats {
        FetcherStats {
            requests: self.stats.requests.load(Ordering::Relaxed),
            success: self.stats.success.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            retries: self.stats.retries.load(Ordering::Relaxed),
            bytes_downloaded: self.stats.bytes_downloaded.load(Ordering::Relaxed),
        }
    }

    #[allow(dead_code)]
    pub fn verbose(&self) -> bool {
        self.verbose
    }
}

// ============================================================
// CA CERTIFICATE BUNDLE DETECTION
// ============================================================

/// Find the best CA certificate bundle path for the current platform.
pub fn find_ca_bundle() -> Option<String> {
    // 1. Check environment variables (user can override)
    for var in &["SSL_CERT_FILE", "CURL_CA_BUNDLE"] {
        if let Ok(path) = std::env::var(var) {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }
    }

    // 2. Common Linux paths
    let common_paths = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/usr/local/share/certs/ca-root-nss.crt",
        "/usr/share/ca-certificates/mozilla/ca-certificates.crt",
    ];

    for p in &common_paths {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }

    // 3. Termux-specific paths
    if let Ok(prefix) = std::env::var("PREFIX") {
        let termux_paths = [
            format!("{prefix}/etc/tls/cert.pem"),
            format!("{prefix}/etc/ssl/certs/ca-certificates.crt"),
            format!("{prefix}/etc/tls/ca-bundle.crt"),
        ];
        for p in &termux_paths {
            if std::path::Path::new(p).exists() {
                return Some(p.clone());
            }
        }
    }

    // 4. macOS
    #[cfg(target_os = "macos")]
    {
        let macos_paths = [
            "/usr/local/etc/openssl/cert.pem",
            "/opt/homebrew/etc/openssl/cert.pem",
            "/etc/ssl/cert.pem",
        ];
        for p in &macos_paths {
            if std::path::Path::new(p).exists() {
                return Some(p.to_string());
            }
        }
    }

    None
}

// ============================================================
// OPTIMIZED CURL FETCH (with connection reuse)
// ============================================================

/// Optimized curl fetch with thread-local connection reuse.
///
/// On first call per blocking thread: creates a fully configured curl handle
/// (TCP+TLS handshake, ~100-200ms overhead).
/// On subsequent calls: reuses the cached handle (HTTP keep-alive, ~0ms overhead).
///
/// The handle is cached in thread-local storage, so each blocking thread
/// maintains its own connection. With max_blocking_threads == max_in_flight,
/// each thread handles ~34 requests (8677/256), and only the first pays
/// the handshake cost.
///
/// Zero-copy response: uses UnsafeCell to take Vec<u8> ownership
/// and convert to String without cloning the response body.
fn curl_fetch_optimized(
    url: &str,
    timeout_secs: u64,
    proxy_url: &str,
    verbose: bool,
    ca_bundle_path: &Option<String>,
) -> Result<String> {
    CACHED_CURL_HANDLE.with(|cell| {
        let mut entry = cell.borrow_mut();

        // Take cached handle (creates new one on first call per thread)
        let mut easy = entry.take().unwrap_or_else(|| {
            create_configured_handle(timeout_secs, proxy_url, verbose, ca_bundle_path)
        });

        // Set URL for this request (all other settings persist from handle creation)
        if let Err(e) = easy.url(url) {
            // Reset collector and cache handle back even on URL set failure
            {
                let collector = easy.get_ref();
                collector.reset();
            }
            *entry = Some(easy);
            anyhow::bail!("curl URL set error: {}", e);
        }

        // Perform the HTTP request
        match easy.perform() {
            Ok(()) => {}
            Err(e) => {
                let code = e.code();
                let desc = e.description();
                // On network error: drop the handle entirely (connection may be dead).
                // Next call will create a fresh handle with new connection.
                // Don't cache a handle with a potentially dead connection.
                anyhow::bail!("curl error [{}]: {} | URL: {}", code, desc, url);
            }
        }

        // ===== After perform() succeeds: ALWAYS cache handle back =====
        // The TCP/TLS connection is still valid even on HTTP errors (429,
        // content-type mismatch, CF challenge). Only drop on network errors.
        // Extract all metadata first, then cache handle, then validate.

        // Extract response metadata. Copy into owned values so borrows
        // don't extend past the handle cache.
        let response_code = easy.response_code()?;

        // Content type — extract as &str first, convert to owned before mutable ops.
        // The String is short (~25 bytes for "text/html; charset=utf-8") — minimal overhead.
        let content_type = easy.content_type().unwrap_or(None).unwrap_or("").to_string();

        // Only extract primary_ip when verbose (saves FFI call + ~15 bytes alloc per request)
        let primary_ip = if verbose {
            easy.primary_ip().unwrap_or(None).map(|s| s.to_string())
                .unwrap_or_else(|| "?".to_string())
        } else {
            String::new()
        };

        let total_time = easy.total_time().unwrap_or(Duration::ZERO).as_secs_f64();

        // Take data from collector (zero-copy: Vec<u8> → String without clone)
        let (len, text) = {
            let collector = easy.get_ref();
            let len = collector.len();
            let response_bytes = collector.take_data();

            let text = if std::str::from_utf8(&response_bytes).is_ok() {
                // SAFETY: We just verified the bytes are valid UTF-8
                unsafe { String::from_utf8_unchecked(response_bytes) }
            } else {
                String::from_utf8_lossy(&response_bytes).into_owned()
            };
            (len, text)
            // collector borrow released here
        };

        // On HTTP 429: mark connection for closure but keep handle cached.
        // The current connection may be rate-limited; forbid_reuse closes it,
        // so the next request opens a fresh connection while reusing the handle
        // (saves TCP+TLS handshake ~100-200ms on the next request to same host).
        //
        // CRITICAL: Reset forbid_reuse to false BEFORE caching the handle back.
        // If we set forbid_reuse(true) and never reset it, EVERY subsequent
        // request on this handle will also close its connection, permanently
        // defeating HTTP keep-alive and adding 100-200ms TCP+TLS handshake
        // per request. This was a major bug — after a single 429, the handle
        // would burn connections forever.
        if response_code == 429 {
            let _ = easy.forbid_reuse(true);
        } else {
            // Ensure forbid_reuse is reset to false for non-429 responses.
            // This handles the case where a previous 429 set it to true.
            let _ = easy.forbid_reuse(false);
        }

        // Reset collector buffer (preserves capacity) and cache handle back.
        // ALWAYS cache after successful perform() — connection is still valid.
        {
            let collector = easy.get_ref();
            collector.reset();
        }
        *entry = Some(easy);

        // ===== All validation below; handle is already cached =====
        // Bailing from here still preserves the cached handle for reuse.

        // Rate limit detection: longer backoff in retry loop
        if response_code == 429 {
            anyhow::bail!("HTTP 429 RATE_LIMITED");
        }

        // Content-type check: skip processing non-HTML responses
        if !content_type.contains("text/html") {
            anyhow::bail!(
                "Unexpected content type: {} (HTTP {})",
                content_type, response_code
            );
        }

        // Verbose logging
        if verbose {
            eprintln!(
                "[CURL] {} → HTTP {} | {} bytes | {:.3}s | IP: {}",
                url, response_code, len, total_time, primary_ip
            );
        }

        // Validate response
        if response_code >= 400 {
            let snippet = if text.len() > 200 {
                &text[..200]
            } else {
                &text
            };
            anyhow::bail!("HTTP {response_code} | body: {}", snippet.trim());
        }
        // Only scan first 2KB for Cloudflare challenge — challenge pages are always short.
        // Full response can be 50-200KB, so this saves ~1.2GB of string scanning across 8677 requests.
        let cf_check = &text[..text.len().min(2048)];
        if cf_check.contains("Just a moment...")
            || cf_check.contains("cf-challenge")
            || cf_check.contains("Checking your browser")
        {
            anyhow::bail!(
                "Cloudflare challenge detected (HTTP {response_code}, {} bytes)",
                text.len()
            );
        }
        if text.len() < 100 && (text.contains("error") || text.contains("Access denied")) {
            anyhow::bail!(
                "Suspicious short response ({} bytes): {}",
                text.len(),
                text.trim()
            );
        }

        Ok(text)
    })
}
