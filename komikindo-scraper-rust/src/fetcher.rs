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
// FETCHER (NO SEMAPHORE - FULL SPEED)
// ============================================================

pub struct Fetcher {
    max_retries: u32,
    timeout_secs: u64,
    proxy_url: String,
    stats: Arc<AtomicFetcherStats>,
    start_time: Instant,
}

impl Fetcher {
    pub fn new(timeout_secs: u64, proxy_url: Option<&str>) -> Result<Self> {
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
            "[FETCHER] Started (libcurl, NO LIMIT): timeout={}s{}",
            timeout_secs, proxy_info
        );

        Ok(Fetcher {
            max_retries: cfg.scraper_retries,
            timeout_secs,
            proxy_url: effective_proxy,
            stats: Arc::new(AtomicFetcherStats::default()),
            start_time: Instant::now(),
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

        tokio::task::spawn_blocking(move || {
            let mut last_err = String::new();

            for attempt in 0..max_retries {
                match curl_fetch(&url, timeout_secs, &proxy_url) {
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

            handles.push(tokio::task::spawn_blocking(move || {
                let mut last_err = String::new();
                for attempt in 0..max_retries {
                    match curl_fetch(&url, timeout_secs, &proxy_url) {
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
}

// ============================================================
// CURL FETCH (per-request)
// ============================================================

fn curl_fetch(url: &str, timeout_secs: u64, proxy_url: &str) -> Result<String> {
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

    // Connection reuse (keep-alive)

    handle.perform()?;
    let response_code = handle.response_code()?;

    let collector = handle.get_ref();
    let text = String::from_utf8_lossy(&collector.data).to_string();

    if response_code >= 400 {
        anyhow::bail!("HTTP {response_code}");
    }
    if text.contains("Just a moment...")
        || text.contains("cf-challenge")
        || text.contains("Checking your browser")
    {
        anyhow::bail!("Cloudflare challenge detected (status={response_code})");
    }
    if text.len() < 100 && (text.contains("error") || text.contains("Access denied")) {
        anyhow::bail!("Suspicious short response ({} bytes)", text.len());
    }

    Ok(text)
}
