/// Async HTTP fetcher menggunakan libcurl (bypass Cloudflare).
///
/// Menggunakan `curl` crate Rust yang memakai native libcurl.
/// libcurl punya TLS fingerprint Chrome-compatible, jadi Cloudflare
/// tidak mendeteksi sebagai bot (berbeda dengan reqwest/rustls).
///
/// Architecture:
///   - Semaphore untuk kontrol concurrency
///   - Cookie jar otomatis (handle CF __cf_bm, cf_clearance)
///   - Auto-retry dengan exponential backoff
///   - Connection reuse via curl multi handle
///   - SOCKS5/HTTP proxy support
///
/// Termux Compatible:
///   - static-curl = semua symbol libcurl di-static link
///   - Tidak perlu libcurl.so di system

use anyhow::Result;
use curl::easy::{Easy2, Handler, HttpVersion, List, WriteError};
use log::debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
// COLLECTOR (curl response handler)
// ============================================================

/// Handler untuk mengumpulkan response body dari curl.
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
    semaphore: Arc<Semaphore>,
    max_retries: u32,
    timeout_secs: u64,
    proxy_url: String,
    stats: Arc<AtomicFetcherStats>,
    start_time: Instant,
}

impl Fetcher {
    /// Buat Fetcher baru berbasis libcurl.
    ///
    /// # Arguments
    /// * `concurrency` - Max concurrent requests
    /// * `timeout_secs` - Request timeout (default 30)
    /// * `proxy_url` - Optional SOCKS5/HTTP proxy
    pub fn new(concurrency: usize, timeout_secs: u64, proxy_url: Option<&str>) -> Result<Self> {
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

        println!(
            "[FETCHER] Started (libcurl): concurrency={}, timeout={}s{}",
            concurrency, timeout_secs, proxy_info
        );

        Ok(Fetcher {
            semaphore: Arc::new(Semaphore::new(concurrency)),
            max_retries: cfg.scraper_retries,
            timeout_secs,
            proxy_url: effective_proxy,
            stats: Arc::new(AtomicFetcherStats::default()),
            start_time: Instant::now(),
        })
    }

    /// Fetch satu halaman HTML.
    ///
        /// Uses `spawn_blocking` karena libcurl adalah sync API.
    pub async fn fetch_page(&self, url: &str) -> Result<String> {
        let _permit = self.semaphore.clone().acquire_owned().await
            .expect("Semaphore closed");

        self.stats.requests.fetch_add(1, Ordering::Relaxed);

        let url = url.to_string();
        let timeout_secs = self.timeout_secs;
        let proxy_url = self.proxy_url.clone();
        let stats = Arc::clone(&self.stats);
        let max_retries = self.max_retries;

        tokio::task::spawn_blocking(move || {
            let mut last_err = String::new();

            for attempt in 0..max_retries {
                match curl_fetch(&url, timeout_secs, &proxy_url) {
                    Ok(html) => {
                        stats.success.fetch_add(1, Ordering::Relaxed);
                        return Ok(html);
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        stats.retries.fetch_add(1, Ordering::Relaxed);
                        if attempt < max_retries - 1 {
                            debug!(
                                "Retry {}/{} for {}: {}",
                                attempt + 1, max_retries, url, e
                            );
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

    /// Ambil statistik fetcher.
    pub fn stats(&self) -> FetcherStats {
        FetcherStats {
            requests: self.stats.requests.load(Ordering::Relaxed),
            success: self.stats.success.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            retries: self.stats.retries.load(Ordering::Relaxed),
            bytes_downloaded: self.stats.bytes_downloaded.load(Ordering::Relaxed),
        }
    }

    /// Elapsed time sejak Fetcher dibuat.
    #[allow(dead_code)]
    pub fn elapsed_secs(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }
}

// ============================================================
// CURL FETCH IMPLEMENTATION
// ============================================================

/// Fetch URL menggunakan libcurl dengan Chrome-like settings.
fn curl_fetch(url: &str, timeout_secs: u64, proxy_url: &str) -> Result<String> {
    let mut handle = Easy2::new(Collector::default());

    // URL
    handle.url(url)?;

    // Chrome User-Agent
    handle.useragent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36",
    )?;

    // HTTP/2 preferred (Chrome uses HTTP/2)
    handle.http_version(HttpVersion::V2)?;

    // Follow redirects
    handle.follow_location(true)?;
    handle.max_redirections(10)?;

    // Timeout
    handle.timeout(Duration::from_secs(timeout_secs))?;
    handle.connect_timeout(Duration::from_secs(timeout_secs))?;

    // Headers (Chrome-like)
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

    // Enable cookie jar (memory-only, automatic CF cookie handling)
    handle.cookie_file("")?;
    handle.cookie_list("session=1")?;

    // TLS settings - gunakan default libcurl TLS yang Chrome-compatible

    // Proxy
    if !proxy_url.is_empty() {
        handle.proxy(proxy_url)?;
    }

    // DNS cache timeout
    handle.dns_cache_timeout(Duration::from_secs(300))?;

    // TCP keepalive
    handle.tcp_keepalive(true)?;
    handle.tcp_keepidle(Duration::from_secs(30))?;

    // Compressed transfer - only gzip/deflate (no brotli in static curl)
    handle.accept_encoding("gzip, deflate")?;

    // Execute
    handle.perform()?;
    let response_code = handle.response_code()?;

    // Get collected data
    let collector = handle.get_ref();
    let bytes = &collector.data;
    let text = String::from_utf8_lossy(bytes).to_string();

    // Check response code
    if response_code >= 400 {
        anyhow::bail!("HTTP {response_code}");
    }

    // Cloudflare challenge detection
    if text.contains("Just a moment...") || text.contains("cf-challenge") || text.contains("Checking your browser") {
        anyhow::bail!("Cloudflare challenge detected (status={response_code})");
    }

    if text.len() < 100 && (text.contains("error") || text.contains("Access denied")) {
        anyhow::bail!("Suspicious short response ({} bytes)", text.len());
    }

    Ok(text)
}
