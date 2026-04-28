/// Async HTTP fetcher menggunakan libcurl (bypass Cloudflare).
///
/// FULL SPEED MODE:
///   - Tidak ada semaphore / concurrency limit
///   - Bottleneck hanya di internet (bandwidth + latency)
///   - spawn_blocking dengan max_blocking_threads besar
///
/// Architecture:
///   - Setiap request = 1 thread di blocking pool (libcurl sync API)
///   - Cookie jar per-handle (CF __cf_bm cookies)
///   - Auto-retry dengan exponential backoff
///   - SOCKS5/HTTP proxy support
///
/// DEBUG MODE:
///   - `--verbose` enables libcurl verbose output (protocol details to stderr)
///   - Every fetch logs: HTTP status, bytes, time, remote IP
///   - Errors always include curl error code + description

use anyhow::Result;
use curl::easy::{Easy2, Handler, HttpVersion, List, WriteError};
use log::debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
// COLLECTOR (curl response handler)
// ============================================================

#[derive(Default)]
struct Collector {
    data: Vec<u8>,
}

impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.data.extend_from_slice(data);
        Ok(data.len())
    }
}

// ============================================================
// FETCHER
// ============================================================

pub struct Fetcher {
    max_retries: u32,
    timeout_secs: u64,
    proxy_url: String,
    stats: Arc<AtomicFetcherStats>,
    start_time: Instant,
    verbose: bool,
}

impl Fetcher {
    pub fn new(timeout_secs: u64, proxy_url: Option<&str>, verbose: bool) -> Result<Self> {
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

        // Log CA bundle status
        let ca_info = match find_ca_bundle() {
            Some(ref p) => format!("\n  CA bundle: {}", p),
            None => "\n  CA bundle: NOT FOUND (SSL will fail!)".to_string(),
        };

        println!(
            "[FETCHER] Started (libcurl, NO LIMIT): timeout={}s{}{}{}",
            timeout_secs,
            proxy_info,
            if verbose { " [VERBOSE]" } else { "" },
            ca_info
        );

        Ok(Fetcher {
            max_retries: cfg.scraper_retries,
            timeout_secs,
            proxy_url: effective_proxy,
            stats: Arc::new(AtomicFetcherStats::default()),
            start_time: Instant::now(),
            verbose,
        })
    }

    /// Fetch satu halaman. Tidak ada semaphore - langsung spawn_blocking.
    pub async fn fetch_page(&self, url: &str) -> Result<String> {
        self.stats.requests.fetch_add(1, Ordering::Relaxed);

        let url = url.to_string();
        let timeout_secs = self.timeout_secs;
        let proxy_url = self.proxy_url.clone();
        let stats = Arc::clone(&self.stats);
        let max_retries = self.max_retries;
        let verbose = self.verbose;

        tokio::task::spawn_blocking(move || {
            let mut last_err = String::new();

            for attempt in 0..max_retries {
                match curl_fetch(&url, timeout_secs, &proxy_url, verbose) {
                    Ok(html) => {
                        stats.success.fetch_add(1, Ordering::Relaxed);
                        stats.bytes_downloaded
                            .fetch_add(html.len() as u64, Ordering::Relaxed);
                        return Ok(html);
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        stats.retries.fetch_add(1, Ordering::Relaxed);
                        if attempt < max_retries - 1 {
                            let msg = format!("Retry {}/{} for {}: {}", attempt + 1, max_retries, url, e);
                            if verbose {
                                eprintln!("[FETCH] {msg}");
                            } else {
                                debug!("{msg}");
                            }
                            std::thread::sleep(Duration::from_secs_f64(
                                0.5 * (attempt + 1) as f64,
                            ));
                        }
                    }
                }
            }

            stats.failed.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!(
                "Gagal fetch {} setelah {} retries. Last: {}",
                url, max_retries, last_err
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("Task error: {e}"))?
    }

    /// Batch fetch - untuk banyak URL sekaligus tanpa limit.
    pub async fn fetch_pages_batch(&self, urls: &[String]) -> Vec<(String, Result<String>)> {
        let mut handles = Vec::with_capacity(urls.len());
        for url in urls {
            let url = url.clone();
            let stats = Arc::clone(&self.stats);
            stats.requests.fetch_add(1, Ordering::Relaxed);

            let timeout_secs = self.timeout_secs;
            let proxy_url = self.proxy_url.clone();
            let max_retries = self.max_retries;
            let verbose = self.verbose;

            handles.push(tokio::task::spawn_blocking(move || {
                let mut last_err = String::new();
                for attempt in 0..max_retries {
                    match curl_fetch(&url, timeout_secs, &proxy_url, verbose) {
                        Ok(html) => {
                            stats.success.fetch_add(1, Ordering::Relaxed);
                            stats.bytes_downloaded
                                .fetch_add(html.len() as u64, Ordering::Relaxed);
                            return (url, Ok(html));
                        }
                        Err(e) => {
                            last_err = e.to_string();
                            stats.retries.fetch_add(1, Ordering::Relaxed);
                            if attempt < max_retries - 1 {
                                std::thread::sleep(Duration::from_secs_f64(
                                    0.5 * (attempt + 1) as f64,
                                ));
                            }
                        }
                    }
                }
                stats.failed.fetch_add(1, Ordering::Relaxed);
                (url, Err(anyhow::anyhow!(
                    "Gagal setelah {} retries: {}", max_retries, last_err
                )))
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(r) => results.push(r),
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

    pub fn verbose(&self) -> bool {
        self.verbose
    }
}

// ============================================================
// CA CERTIFICATE BUNDLE DETECTION
// ============================================================

/// Find the best CA certificate bundle path for the current platform.
/// Required when curl is built with static-curl + rustls (no default CA path).
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
        "/etc/ssl/certs/ca-certificates.crt",                     // Debian/Ubuntu
        "/etc/pki/tls/certs/ca-bundle.crt",                       // RHEL/CentOS/Fedora
        "/etc/ssl/ca-bundle.pem",                                  // OpenSUSE
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",      // Newer RHEL/Fedora
        "/usr/local/share/certs/ca-root-nss.crt",                 // FreeBSD/Nix
        "/usr/share/ca-certificates/mozilla/ca-certificates.crt", // some Linux
    ];

    for p in &common_paths {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }

    // 3. Termux-specific paths ($PREFIX usually = /data/data/com.termux/files/usr)
    if let Ok(prefix) = std::env::var("PREFIX") {
        let termux_paths = [
            format!("{}/etc/tls/cert.pem"),
            format!("{}/etc/ssl/certs/ca-certificates.crt"),
            format!("{}/etc/tls/ca-bundle.crt"),
        ];
        for p in &termux_paths {
            if std::path::Path::new(p).exists() {
                return Some(p.clone());
            }
        }
    }

    // 4. macOS (for completeness)
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

/// Initialize CA bundle for a curl handle. Logs result in verbose mode.
fn configure_ca_bundle(handle: &mut Easy2<Collector>, verbose: bool) {
    match find_ca_bundle() {
        Some(path) => {
            match handle.ssl_ca_info(&path) {
                Ok(_) => {
                    if verbose {
                        eprintln!("[CURL] CA bundle: {}", path);
                    }
                }
                Err(e) => {
                    eprintln!("[CURL] WARNING: Failed to set CA bundle '{}': {}", path, e);
                }
            }
        }
        None => {
            eprintln!("[CURL] WARNING: No CA certificate bundle found!");
            eprintln!("[CURL] SSL connections WILL FAIL. Fix:");
            eprintln!("[CURL]   1. Install ca-certificates: pkg install ca-certificates (Termux)");
            eprintln!("[CURL]   2. Or set env: export SSL_CERT_FILE=/path/to/ca-bundle.crt");
            eprintln!("[CURL]   3. Or set env: export CURL_CA_BUNDLE=/path/to/ca-bundle.crt");
        }
    }
}

// ============================================================
// CURL FETCH (per-request)
// ============================================================

fn curl_fetch(url: &str, timeout_secs: u64, proxy_url: &str, verbose: bool) -> Result<String> {
    let mut handle = Easy2::new(Collector::default());

    handle.url(url)?;
    handle.useragent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36",
    )?;
    handle.http_version(HttpVersion::V2)?;
    handle.follow_location(true)?;
    handle.max_redirections(10)?;
    handle.timeout(Duration::from_secs(timeout_secs))?;
    handle.connect_timeout(Duration::from_secs(timeout_secs))?;

    let mut headers = List::new();
    headers.append("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8")?;
    headers.append("Accept-Language: id-ID,id;q=0.9,en-US;q=0.8,en;q=0.7")?;
    headers.append(&format!("Referer: {BASE_URL}/"))?;
    headers.append("Accept-Encoding: gzip, deflate")?;
    headers.append("Sec-Fetch-Dest: document")?;
    headers.append("Sec-Fetch-Mode: navigate")?;
    headers.append("Sec-Fetch-Site: none")?;
    headers.append("Sec-Fetch-User: ?1")?;
    headers.append("Sec-Ch-Ua: \"Not_A Brand\";v=\"8\", \"Chromium\";v=\"120\"")?;
    headers.append("Sec-Ch-Ua-Mobile: ?0")?;
    headers.append("Sec-Ch-Ua-Platform: \"Windows\"")?;
    headers.append("Upgrade-Insecure-Requests: 1")?;
    handle.http_headers(headers)?;

    handle.cookie_file("")?;
    handle.cookie_list("session=1")?;

    if !proxy_url.is_empty() {
        handle.proxy(proxy_url)?;
    }

    handle.dns_cache_timeout(Duration::from_secs(300))?;
    handle.tcp_keepalive(true)?;
    handle.tcp_keepidle(Duration::from_secs(30))?;
    handle.accept_encoding("gzip, deflate")?;

    // Configure CA certificate bundle (required for static-curl + rustls)
    configure_ca_bundle(&mut handle, verbose);

    // Enable curl verbose output (protocol details → stderr)
    if verbose {
        handle.verbose(true)?;
    }

    match handle.perform() {
        Ok(()) => {}
        Err(e) => {
            let code = e.code();
            let desc = e.description();
            // Build detailed error message
            let extra = if verbose {
                let os_errno = handle.os_errno().unwrap_or(0);
                format!(" | os_errno={}", os_errno)
            } else {
                String::new()
            };
            anyhow::bail!(
                "curl error [{}]: {} | URL: {}{}",
                code, desc, url, extra
            );
        }
    }

    let response_code = handle.response_code()?;
    let total_time = handle.total_time().unwrap_or(Duration::ZERO).as_secs_f64();
    let primary_ip = handle.primary_ip().unwrap_or(None).unwrap_or("?");
    let namelookup_time = handle.namelookup_time().unwrap_or(Duration::ZERO).as_secs_f64();
    let connect_time = handle.connect_time().unwrap_or(Duration::ZERO).as_secs_f64();

    let collector = handle.get_ref();
    let len = collector.data.len();
    let text = String::from_utf8_lossy(&collector.data).to_string();

    if verbose {
        eprintln!(
            "[CURL] {} → HTTP {} | {} bytes | {:.3}s (dns={:.3}s conn={:.3}s) | IP: {}",
            url, response_code, len, total_time, namelookup_time, connect_time, primary_ip
        );
    }

    if response_code >= 400 {
        // Log response body snippet on error
        let snippet = if text.len() > 200 { &text[..200] } else { &text };
        anyhow::bail!("HTTP {response_code} | body: {}", snippet.trim());
    }
    if text.contains("Just a moment...")
        || text.contains("cf-challenge")
        || text.contains("Checking your browser")
    {
        anyhow::bail!("Cloudflare challenge detected (HTTP {response_code}, {} bytes)", text.len());
    }
    if text.len() < 100 && (text.contains("error") || text.contains("Access denied")) {
        anyhow::bail!("Suspicious short response ({} bytes): {}", text.len(), text.trim());
    }

    Ok(text)
}
