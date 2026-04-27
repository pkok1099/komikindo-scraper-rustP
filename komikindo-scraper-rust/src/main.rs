/// KomikIndo Scraper - Rust Version
///
/// Full Fetch: Scrape seluruh komik + detail + chapter images (lokal JSONL)
///
/// PERBAIKAN SLUG:
///   - Chapter URL diambil langsung dari href di halaman detail komik
///   - TIDAK lagi construct manual dari slug + chapter_number
///   - Ini menghindari masalah slug yang tidak match dengan format URL asli
///
/// Termux Compatible:
///   - rustls-tls (no OpenSSL dependency)
///   - cross-compile atau build langsung di Termux

mod config;
mod fetcher;
mod jsonl;
mod parsers;
mod scraper;

use anyhow::Result;
use chrono::{Local, DateTime};
use clap::Parser;
use parsers::KomikDetail;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

use crate::config::env_config;
use crate::fetcher::Fetcher;
use crate::scraper::{scrape_chapter_images, scrape_full_komik_list, scrape_komik_detail};

// ============================================================
// CLI
// ============================================================

#[derive(Parser, Debug)]
#[command(name = "komikindo-scraper")]
#[command(about = "KomikIndo Scraper - Rust Version (Termux Compatible)")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Full fetch semua komik + detail + chapter images (lokal JSONL)
    FullFetch {
        /// Skip scraping chapter images (lebih cepat)
        #[arg(long)]
        skip_chapters: bool,

        /// Limit jumlah komik (0 = semua)
        #[arg(long, default_value_t = 0)]
        limit: usize,

        /// Resume dari slug tertentu
        #[arg(long)]
        start_from: Option<String>,

        /// Resume dari JSONL terakhir
        #[arg(long)]
        resume: bool,

        /// Max concurrent HTTP requests
        #[arg(long, default_value_t = 200)]
        concurrency: usize,

        /// Turbo mode: 500 concurrency
        #[arg(long)]
        turbo: bool,

        /// SOCKS5/HTTP proxy URL
        #[arg(long)]
        proxy: Option<String>,
    },

    /// Scrape homepage untuk cek update terbaru
    Homepage {
        /// SOCKS5/HTTP proxy URL
        #[arg(long)]
        proxy: Option<String>,
    },
}

// ============================================================
// MAIN
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::FullFetch {
            skip_chapters,
            limit,
            start_from,
            resume,
            concurrency,
            turbo,
            proxy,
        } => {
            let concurrency = if turbo { 500 } else { concurrency };
            if turbo {
                println!("[TURBO MODE] 500 concurrency");
            }

            run_full_fetch(FullFetchOpts {
                skip_chapters,
                limit,
                start_from,
                resume,
                concurrency,
                proxy,
            })
            .await?;
        }
        Commands::Homepage { proxy } => {
            let fetcher = Fetcher::new(50, 30, proxy.as_deref())?;
            let updates = scraper::scrape_homepage_updates(&fetcher).await?;
            println!("\n=== Homepage Updates ({}) ===", updates.len());
            for u in &updates {
                println!(
                    "  {} | {} | ch.{:?}",
                    u.slug,
                    u.judul.as_deref().unwrap_or("?"),
                    u.latest_chapter_number,
                );
            }
        }
    }

    Ok(())
}

struct FullFetchOpts {
    skip_chapters: bool,
    limit: usize,
    start_from: Option<String>,
    resume: bool,
    concurrency: usize,
    proxy: Option<String>,
}

async fn run_full_fetch(opts: FullFetchOpts) -> Result<()> {
    let start_time = Instant::now();
    let dt_start = Local::now();

    // Data directory
    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;

    // Proxy info
    let proxy_url = opts.proxy.as_deref();
    let cfg = env_config();
    let proxy_info = if let Some(p) = proxy_url {
        format!("\n  Proxy: {p}")
    } else if cfg.proxy_enabled && !cfg.proxy_url.is_empty() {
        format!("\n  Proxy: {}", cfg.proxy_url)
    } else {
        String::new()
    };

    println!("{}", "=".repeat(70));
    println!(
        "  KOMIKINDO FULL FETCH (RUST + ASYNC + JSONL)"
    );
    println!(
        "  Started at {}",
        dt_start.format("%Y-%m-%d %H:%M:%S")
    );
    println!(
        "  Concurrency: {} | Skip chapters: {}",
        opts.concurrency, opts.skip_chapters
    );
    println!("{proxy_info}");
    println!("{}", "=".repeat(70));

    // === Determine output file and resume point ===
    let mut resume_slug = opts.start_from.unwrap_or_default();

    let jsonl_path = if opts.resume {
        if let Some(latest) = jsonl::find_latest_jsonl(&data_dir) {
            let already_done = jsonl::count_jsonl(&latest);
            match jsonl::last_slug_from_jsonl(&latest) {
                Some(slug) => {
                    resume_slug = slug;
                    println!(
                        "[RESUME] Found {}, {} komik done, last: {}",
                        latest.display(),
                        already_done,
                        resume_slug
                    );
                    latest
                }
                None => {
                    println!(
                        "[RESUME] Found {} but no valid last slug",
                        latest.display()
                    );
                    create_jsonl_path(&data_dir, &dt_start)
                }
            }
        } else {
            println!("[RESUME] No existing JSONL found, starting fresh");
            create_jsonl_path(&data_dir, &dt_start)
        }
    } else {
        create_jsonl_path(&data_dir, &dt_start)
    };

    // === Create fetcher ===
    let fetcher = Arc::new(Fetcher::new(opts.concurrency, 30, proxy_url)?);
    let chapter_semaphore = Arc::new(Semaphore::new(50));

    // === Step 1: Fetch all komik slugs ===
    println!("\n--- Step 1: Fetching komik list ---");
    let komik_list = scrape_full_komik_list(&fetcher).await?;
    if komik_list.is_empty() {
        println!("No komik found! Exiting.");
        return Ok(());
    }

    let mut komik_list = komik_list;
    println!("Total komik found: {}", komik_list.len());

    // Apply limit
    if opts.limit > 0 && komik_list.len() > opts.limit {
        komik_list.truncate(opts.limit);
        println!("Limited to first {} komik", opts.limit);
    }

    // Apply resume/start_from
    if !resume_slug.is_empty() {
        let found = komik_list.iter().position(|s| s == &resume_slug);
        match found {
            Some(idx) => {
                komik_list = komik_list.split_off(idx + 1);
                println!(
                    "Resuming after '{}': {} komik remaining",
                    resume_slug,
                    komik_list.len()
                );
            }
            None => {
                println!(
                    "Warning: slug '{}' not found, starting from beginning",
                    resume_slug
                );
            }
        }
    }

    let total = komik_list.len();
    println!("\nKomik to process: {total}");

    if total == 0 {
        println!("Nothing to process.");
        return Ok(());
    }

    // === Step 2: Fetch details + chapter images ===
    println!("\n--- Step 2: Fetching details + chapter images ---");
    println!(
        "[INFO] Menggunakan chapter URL langsung dari detail page (fix slug issue)"
    );

    let mut success: usize = 0;
    let mut failed: usize = 0;
    let mut total_ch_images: usize = 0;
    let mut total_chapters: usize = 0;

    let batch_size = 20;

    for batch_start in (0..total).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(total);
        let batch = &komik_list[batch_start..batch_end];

        // --- Fetch all komik details concurrently ---
        let mut detail_handles = Vec::with_capacity(batch.len());
        for slug in batch {
            let fetcher = Arc::clone(&fetcher);
            let slug = slug.clone();
            detail_handles.push(tokio::spawn(async move {
                let result = scrape_komik_detail(&slug, &fetcher).await;
                (slug, result)
            }));
        }

        let mut detail_results: Vec<(String, Result<KomikDetail>)> =
            Vec::with_capacity(detail_handles.len());
        for handle in detail_handles {
            match handle.await {
                Ok(result) => detail_results.push(result),
                Err(e) => {
                    detail_results.push((
                        "unknown".into(),
                        Err(anyhow::anyhow!("Task error: {e}")),
                    ));
                }
            }
        }

        // --- Fetch chapter images menggunakan URL LANGSUNG dari detail page ---
        // INI ADALAH FIX UNTUK MASALAH SLUG!
        // Sebelumnya: build_chapter_url(slug, chapter_number) -> URL bisa salah
        // Sekarang: pakai chapter.url yang sudah diambil dari href di detail page
        if !opts.skip_chapters {
            let mut ch_tasks: Vec<tokio::task::JoinHandle<(usize, usize, Result<parsers::ChapterImageData>)>> =
                Vec::new();

            for (detail_idx, (_slug, detail_result)) in detail_results.iter().enumerate() {
                if let Ok(detail) = detail_result {
                    for (ch_idx, chapter) in detail.chapters.iter().enumerate() {
                        // PAKAI URL LANGSUNG DARI DETAIL PAGE!
                        // Tidak ada lagi build_chapter_url() yang bisa salah!
                        let url = chapter.url.clone();
                        let fetcher = Arc::clone(&fetcher);
                        let semaphore = Arc::clone(&chapter_semaphore);

                        ch_tasks.push(tokio::spawn(async move {
                            let _permit = semaphore.acquire().await.unwrap();
                            let result = scrape_chapter_images(&url, &fetcher).await;
                            (detail_idx, ch_idx, result)
                        }));
                    }
                }
            }

            // Wait for all chapter image fetches & merge results
            for handle in ch_tasks {
                if let Ok((detail_idx, ch_idx, ch_result)) = handle.await {
                    if let Some((_slug, detail_result)) = detail_results.get_mut(detail_idx) {
                        if let Ok(detail) = detail_result {
                            if let Some(chapter) = detail.chapters.get_mut(ch_idx) {
                                if let Ok(data) = ch_result {
                                    chapter.set_image_data(data);
                                    total_ch_images += chapter.total_images.unwrap_or(0) as usize;
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- Write each completed komik to JSONL ---
        for (slug, result) in &detail_results {
            match result {
                Ok(detail) => {
                    success += 1;
                    total_chapters += detail.chapters.len();
                    if let Err(e) = jsonl::append_jsonl(&jsonl_path, detail) {
                        eprintln!("  [WARN] Gagal write JSONL {}: {}", slug, e);
                    }
                }
                Err(e) => {
                    failed += 1;
                    let failed_json = serde_json::json!({
                        "slug": slug,
                        "_status": "detail_failed",
                        "_error": e.to_string(),
                    });
                    let _ = jsonl::append_jsonl_raw(&jsonl_path, &failed_json.to_string());
                }
            }
        }

        // --- Progress ---
        let completed = success + failed;
        let pct = completed as f64 / total as f64 * 100.0;
        let elapsed = start_time.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            completed as f64 / elapsed * 60.0
        } else {
            0.0
        };
        let eta = if rate > 0.0 {
            (total - completed) as f64 / rate
        } else {
            0.0
        };

        let stats = fetcher.stats();
        println!(
            "  [{}/{} {:.1}%] {:.1} komik/min | ETA: {:.0}min | \
             OK: {} FAIL: {} | Ch: {} Img: {} | DL: {:.1}MB",
            completed,
            total,
            pct,
            rate,
            eta,
            success,
            failed,
            total_chapters,
            total_ch_images,
            stats.mb_downloaded(),
        );
    }

    // === Summary ===
    let elapsed = start_time.elapsed().as_secs_f64();
    let stats = fetcher.stats();
    let jsonl_size_mb = std::fs::metadata(&jsonl_path)
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    println!("{}", "=".repeat(70));
    println!("  FULL FETCH COMPLETE");
    println!("  Total komik:    {total}");
    println!("  Success:        {success}");
    println!("  Failed:         {failed}");
    println!("  Chapters:       {total_chapters}");
    println!("  Images:         {total_ch_images}");
    println!(
        "  Time:           {:.1}s ({:.1}min)",
        elapsed,
        elapsed / 60.0
    );
    println!(
        "  Rate:           {:.1} komik/min",
        total as f64 / elapsed * 60.0
    );
    println!("  Downloaded:     {:.1} MB", stats.mb_downloaded());
    println!(
        "  JSONL:          {} ({:.1} MB)",
        jsonl_path.display(),
        jsonl_size_mb
    );
    println!("{}", "=".repeat(70));

    Ok(())
}

fn create_jsonl_path(data_dir: &std::path::Path, dt: &DateTime<Local>) -> PathBuf {
    let timestamp = dt.format("%Y%m%d_%H%M%S").to_string();
    data_dir.join(format!("full_fetch_{timestamp}.jsonl"))
}
