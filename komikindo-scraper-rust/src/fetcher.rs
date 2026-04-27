/// Async HTTP fetcher dengan connection pooling.
/// Mirip AsyncFetcher dari Python version, menggunakan reqwest.
///
/// Termux-compatible: rustls-tls (no OpenSSL), optional SOCKS5 proxy.

use anyhow::{Context, Result};
use log::debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::config::{default_headers, env_config};

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

    pub fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.success as f64 / self.requests as f64 * 100.0
        }
    }
}

/// Async HTTP fetcher dengan connection pooling dan optional proxy.
pub struct Fetcher {
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    max_retries: u32,
    stats: Arc<AtomicFetcherStats>,
    start_time: Instant,
}

#[derive(Default)]
struct AtomicFetcherStats {
    requests: AtomicU64,
    success: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    bytes_downloaded: AtomicU64,
}

impl Fetcher {
    /// Buat Fetcher baru.
    ///
    /// # Arguments
    /// * `concurrency` - Max concurrent requests (default 200)
    /// * `timeout_secs` - Request timeout dalam detik (default 30)
    /// * `proxy_url` - Optional SOCKS5/HTTP proxy URL
    pub fn new(concurrency: usize, timeout_secs: u64, proxy_url: Option<&str>) -> Result<Self> {
        let cfg = env_config();

        let mut builder = reqwest::Client::builder()
            .default_headers(default_headers())
            .timeout(Duration::from_secs(timeout_secs))
            .pool_max_idle_per_host(concurrency.min(100))
            .connection_verbose(false)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .redirect(reqwest::redirect::Policy::limited(10));

        builder = if let Some(proxy) = proxy_url {
            let proxy =
                reqwest::Proxy::all(proxy).context("Gagal membuat proxy")?;
            builder.proxy(proxy)
        } else if cfg.proxy_enabled && !cfg.proxy_url.is_empty() {
            let proxy =
                reqwest::Proxy::all(&cfg.proxy_url).context("Gagal membuat proxy dari env")?;
            builder.proxy(proxy)
        } else {
            builder.no_proxy()
        };

        let client = builder.build().context("Gagal membuat HTTP client")?;

        let proxy_info = proxy_url
            .map(|p| format!(", proxy={p}"))
            .unwrap_or_default();

        println!(
            "[FETCHER] Started: concurrency={}, timeout={}s, pool=ON{}",
            concurrency, timeout_secs, proxy_info
        );

        Ok(Fetcher {
            client,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            max_retries: cfg.scraper_retries,
            stats: Arc::new(AtomicFetcherStats::default()),
            start_time: Instant::now(),
        })
    }

    /// Fetch satu halaman HTML.
    pub async fn fetch_page(&self, url: &str) -> Result<String> {
        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("Semaphore closed");

        self.stats.requests.fetch_add(1, Ordering::Relaxed);

        let mut last_err = String::new();

        for attempt in 0..self.max_retries {
            match self._single_fetch(url).await {
                Ok(html) => {
                    self.stats.success.fetch_add(1, Ordering::Relaxed);
                    return Ok(html);
                }
                Err(e) => {
                    last_err = e.to_string();
                    self.stats.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt < self.max_retries - 1 {
                        let backoff = Duration::from_secs_f64(0.5 * (attempt + 1) as f64);
                        debug!(
                            "Retry {}/{} for {}: {}",
                            attempt + 1,
                            self.max_retries,
                            url,
                            e
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        self.stats.failed.fetch_add(1, Ordering::Relaxed);
        anyhow::bail!(
            "Gagal fetch {} setelah {} retries. Last: {}",
            url,
            self.max_retries,
            last_err
        )
    }

    async fn _single_fetch(&self, url: &str) -> Result<String> {
        let resp = self.client.get(url).send().await?;

        let status = resp.status();
        let text = resp.text().await?;

        // Cloudflare challenge detection
        if text.contains("Just a moment...") || text.contains("cf-challenge") {
            anyhow::bail!("Cloudflare challenge detected (status={status})");
        }

        if !status.is_success() {
            anyhow::bail!("HTTP {status}");
        }

        self.stats
            .bytes_downloaded
            .fetch_add(text.len() as u64, Ordering::Relaxed);
        Ok(text)
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
