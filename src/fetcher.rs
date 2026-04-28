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

        println!(
            "[FETCHER] Started (libcurl, NO LIMIT): timeout={}s{}{}",
            timeout_secs,
            proxy_info,
            if verbose { " [VERBOSE]" } else { "" }
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
