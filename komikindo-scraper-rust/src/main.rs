/// KomikIndo Scraper - Rust Version (FULL SPEED + SMART UPDATE)
///
/// FULL SPEED DESIGN:
///   - Tidak ada semaphore / concurrency limit
///   - Bottleneck hanya di internet (bandwidth + latency)
///   - Streaming pipeline: detail selesai → chapter langsung jalan
///   - tokio blocking pool 8192 threads (1 thread per curl request)
///   - No batch barrier: semua 8671 komik detail + chapter paralel
///
/// SMART UPDATE:
///   - Fetch /komik-terbaru/ untuk cek komik yang baru update
///   - Bandingkan chapter number dengan DB (HashMap, single lookup)
///   - Komik baru → scrape detail (1 request)
///   - Chapter update → construct URL saja (0 request!)
///   - Pagination otomatis: fetch page 2+ kalau item terakhir < 6 jam
///
/// PERBAIKAN SLUG:
///   - Chapter URL diambil langsung dari href di halaman detail komik
///   - Incremental: construct dari url_base (derived dari terbaru page)

mod config;
mod fetcher;
mod jsonl;
mod parsers;
mod scraper;

use anyhow::Result;
use chrono::{DateTime, Local};
use clap::Parser;
use parsers::KomikDetail;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::fetcher::Fetcher;
use crate::scraper::{scrape_chapter_images, scrape_full_komik_list, scrape_komik_detail, scrape_komik_terbaru};

// ============================================================
// CLI
// ============================================================

#[derive(Parser, Debug)]
#[command(name = "komikindo-scraper")]
#[command(about = "KomikIndo Scraper - Rust Full Speed + Smart Update")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Full fetch semua komik + detail + chapter images (FULL SPEED)
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

        /// Timeout per request dalam detik
        #[arg(long, default_value_t = 30)]
        timeout: u64,

        /// SOCKS5/HTTP proxy URL
        #[arg(long)]
        proxy: Option<String>,
    },

    /// Smart incremental update dari /komik-terbaru/
    Update {
        /// Max halaman /komik-terbaru/ yang di-fetch
        #[arg(long, default_value_t = 3)]
        max_pages: u32,

        /// Batas usia entry dalam menit (default 6 jam = 360 menit)
        #[arg(long, default_value_t = 360)]
        max_age_minutes: u32,

        /// Timeout per request dalam detik
        #[arg(long, default_value_t = 30)]
        timeout: u64,

        /// SOCKS5/HTTP proxy URL
        #[arg(long)]
        proxy: Option<String>,

        /// Dry run - hanya tampilkan apa yang akan berubah
        #[arg(long)]
        dry_run: bool,
    },

    /// Scrape homepage untuk cek update terbaru
    Homepage {
        #[arg(long)]
        proxy: Option<String>,
    },
}

// ============================================================
// MAIN (8192 blocking threads)
// ============================================================

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .init();

    let cli = Cli::parse();

    // 8192 blocking threads = 8192 concurrent curl requests
    // Ini agar bottleneck hanya di internet, bukan di thread pool
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(8192)
        .build()?;

    runtime.block_on(async move {
        match cli.command {
            Commands::FullFetch {
                skip_chapters,
                limit,
                start_from,
                resume,
                timeout,
                proxy,
            } => {
                run_full_fetch(FullFetchOpts {
                    skip_chapters,
                    limit,
                    start_from,
                    resume,
                    timeout,
                    proxy,
                })
                .await?;
            }
            Commands::Update {
                max_pages,
                max_age_minutes,
                timeout,
                proxy,
                dry_run,
            } => {
                run_update(UpdateOpts {
                    max_pages,
                    max_age_minutes,
                    timeout,
                    proxy,
                    dry_run,
                })
                .await?;
            }
            Commands::Homepage { proxy } => {
                let fetcher = Fetcher::new(30, proxy.as_deref())?;
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
    })
}

// ============================================================
// FULL FETCH (STREAMING PIPELINE - NO BATCH BARRIER)
// ============================================================
//
// Design:
//   1. Fetch detail untuk SEMUA komik sekaligus (8671 spawn_blocking)
//   2. Setiap detail selesai → langsung spawn chapter fetches
//   3. Setiap chapter selesai → langsung merge ke detail
//   4. Semua chapter detail siap → write ke JSONL
//   5. Tidak ada batch barrier, tidak ada semaphore
//   6. Bottleneck = internet saja

struct FullFetchOpts {
    skip_chapters: bool,
    limit: usize,
    start_from: Option<String>,
    resume: bool,
    timeout: u64,
    proxy: Option<String>,
}

async fn run_full_fetch(opts: FullFetchOpts) -> Result<()> {
    let start_time = Instant::now();
    let dt_start = Local::now();

    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;

    let proxy_url = opts.proxy.as_deref();
    let cfg = config::env_config();
    let proxy_info = if let Some(p) = proxy_url {
        format!("\n  Proxy: {p}")
    } else if cfg.proxy_enabled && !cfg.proxy_url.is_empty() {
        format!("\n  Proxy: {}", cfg.proxy_url)
    } else {
        String::new()
    };

    println!("{}", "=".repeat(70));
    println!("  KOMIKINDO FULL FETCH - FULL SPEED (NO BOTTLENECK)");
    println!("  Started at {}", dt_start.format("%Y-%m-%d %H:%M:%S"));
    println!("  Timeout: {}s | Skip chapters: {}", opts.timeout, opts.skip_chapters);
    println!("  Blocking threads: 8192 (1 per curl request)");
    println!("{proxy_info}");
    println!("{}", "=".repeat(70));

    // === Resume ===
    let mut resume_slug = opts.start_from.unwrap_or_default();

    let jsonl_path = if opts.resume {
        if let Some(latest) = jsonl::find_latest_jsonl(&data_dir) {
            let already_done = jsonl::count_jsonl(&latest);
            match jsonl::last_slug_from_jsonl(&latest) {
                Some(slug) => {
                    resume_slug = slug;
                    println!(
                        "[RESUME] Found {}, {} komik done, last: {}",
                        latest.display(), already_done, resume_slug
                    );
                    latest
                }
                None => {
                    println!("[RESUME] Found {} but no valid last slug", latest.display());
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

    let fetcher = Arc::new(Fetcher::new(opts.timeout, proxy_url)?);

    // === Step 1: Fetch komik list ===
    println!("\n--- Step 1: Fetching komik list ---");
    let komik_list = scrape_full_komik_list(&fetcher).await?;
    if komik_list.is_empty() {
        println!("No komik found! Exiting.");
        return Ok(());
    }

    let mut komik_list = komik_list;
    println!("Total komik found: {}", komik_list.len());

    if opts.limit > 0 && komik_list.len() > opts.limit {
        komik_list.truncate(opts.limit);
        println!("Limited to first {} komik", opts.limit);
    }

    if !resume_slug.is_empty() {
        let found = komik_list.iter().position(|s| s == &resume_slug);
        match found {
            Some(idx) => {
                komik_list = komik_list.split_off(idx + 1);
                println!("Resuming after '{}': {} remaining", resume_slug, komik_list.len());
            }
            None => {
                println!("Warning: slug '{}' not found", resume_slug);
            }
        }
    }

    let total = komik_list.len();
    if total == 0 {
        println!("Nothing to process.");
        return Ok(());
    }
    println!("Komik to process: {total}");

    // === Step 2: FULL SPEED PIPELINE ===
    println!("\n--- Step 2: FULL SPEED PIPELINE (no batch, no semaphore) ---");
    println!("[INFO] Spawning {} detail fetches + unlimited chapter fetches", total);

    let success = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let total_ch_images = Arc::new(AtomicUsize::new(0));
    let total_chapters = Arc::new(AtomicUsize::new(0));
    let jsonl_path = Arc::new(jsonl_path);

    // Spawn ALL detail fetches at once (no batching)
    let mut detail_handles = Vec::with_capacity(total);
    for slug in &komik_list {
        let fetcher = Arc::clone(&fetcher);
        let slug = slug.clone();
        detail_handles.push(tokio::spawn(async move {
            let result = scrape_komik_detail(&slug, &fetcher).await;
            (slug, result)
        }));
    }

    // Use mpsc channel: detail → write JSONL (or spawn chapters → write later)
    let (detail_tx, mut detail_rx) = tokio::sync::mpsc::channel::<DetailResult>(total);

    // Task: collect details, spawn chapters, send results to write
    let write_success = Arc::clone(&success);
    let write_failed = Arc::clone(&failed);
    let write_ch_img = Arc::clone(&total_ch_images);
    let write_ch = Arc::clone(&total_chapters);
    let write_jsonl = Arc::clone(&jsonl_path);
    let skip_ch = opts.skip_chapters;
    let fetcher2 = Arc::clone(&fetcher);

    let writer_task = tokio::spawn(async move {
        // Phase 1: receive all details, spawn chapters immediately
        let mut chapter_join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut pending_writes: Vec<(String, Result<KomikDetail>)> = Vec::new();

        while let Some(mut detail_result) = detail_rx.recv().await {
            if skip_ch {
                // No chapters needed - write immediately
                let path = Arc::clone(&write_jsonl);
                match &detail_result {
                    DetailResult {
                        slug: _,
                        detail: Ok(ref detail),
                    } => {
                        write_ch.fetch_add(detail.chapters.len(), Ordering::Relaxed);
                        write_success.fetch_add(1, Ordering::Relaxed);
                        if let Err(e) = jsonl::append_jsonl(&path, detail) {
                            eprintln!("  [WARN] JSONL write error: {e}");
                        }
                    }
                    DetailResult {
                        slug,
                        detail: Err(e),
                    } => {
                        write_failed.fetch_add(1, Ordering::Relaxed);
                        let failed_json = serde_json::json!({
                            "slug": slug,
                            "_status": "detail_failed",
                            "_error": e.to_string(),
                        });
                        let _ = jsonl::append_jsonl_raw(&path, &failed_json.to_string());
                    }
                }
            } else {
                // Spawn chapter fetches for this detail immediately
                if let Ok(ref mut detail) = detail_result.detail {
                    let chapters: Vec<_> = detail.chapters.iter().enumerate()
                        .filter_map(|(idx, ch)| {
                            if ch.total_images.is_some() {
                                None // Already have images
                            } else {
                                Some((idx, ch.url.clone()))
                            }
                        })
                        .collect();

                    if !chapters.is_empty() {
                        let detail_mut: &mut KomikDetail = detail;
                        let ch_count = chapters.len();

                        // We need to move detail into the spawned task
                        // Clone necessary data
                        let ch_urls: Vec<(usize, String)> = chapters;
                        let mut detail_owned = std::mem::take(detail_mut);
                        // Re-fill slug
                        detail_owned.slug = detail_result.slug.clone();
                        let slug = detail_result.slug.clone();
                        let fetcher = Arc::clone(&fetcher2);
                        let wc = Arc::clone(&write_ch);
                        let wci = Arc::clone(&write_ch_img);
                        let ws = Arc::clone(&write_success);
                        let wf = Arc::clone(&write_failed);
                        let wpath = Arc::clone(&write_jsonl);

                        chapter_join_handles.push(tokio::spawn(async move {
                            // Spawn all chapter fetches
                            let mut ch_handles = Vec::with_capacity(ch_urls.len());
                            for (ch_idx, ch_url) in ch_urls {
                                let fetcher = Arc::clone(&fetcher);
                                ch_handles.push(tokio::spawn(async move {
                                    let result = scrape_chapter_images(&ch_url, &fetcher).await;
                                    (ch_idx, result)
                                }));
                            }

                            // Wait for all chapter fetches
                            for handle in ch_handles {
                                if let Ok((ch_idx, ch_result)) = handle.await {
                                    if let Some(ch) = detail_owned.chapters.get_mut(ch_idx) {
                                        if let Ok(data) = ch_result {
                                            let img_count = data.total_images;
                                            ch.set_image_data(data);
                                            wci.fetch_add(img_count as usize, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }

                            wc.fetch_add(detail_owned.chapters.len(), Ordering::Relaxed);
                            ws.fetch_add(1, Ordering::Relaxed);

                            if let Err(e) = jsonl::append_jsonl(&wpath, &detail_owned) {
                                eprintln!("  [WARN] JSONL write {}: {}", slug, e);
                            }
                        }));

                        // Put placeholder back so we don't double-process
                        detail_result.detail = Err(anyhow::anyhow!("chapters spawned"));
                    } else {
                        // No chapters to fetch, write immediately
                        let path = Arc::clone(&write_jsonl);
                        write_ch.fetch_add(detail.chapters.len(), Ordering::Relaxed);
                        write_success.fetch_add(1, Ordering::Relaxed);
                        if let Err(e) = jsonl::append_jsonl(&path, detail) {
                            eprintln!("  [WARN] JSONL write error: {e}");
                        }
                    }
                } else {
                    // Detail failed
                    let path = Arc::clone(&write_jsonl);
                    write_failed.fetch_add(1, Ordering::Relaxed);
                    let failed_json = serde_json::json!({
                        "slug": detail_result.slug,
                        "_status": "detail_failed",
                        "_error": detail_result.detail.as_ref().err().map(|e| e.to_string()).unwrap_or_default(),
                    });
                    let _ = jsonl::append_jsonl_raw(&path, &failed_json.to_string());
                }
            }
        }

        // Phase 2: wait for all chapter tasks
        for handle in chapter_join_handles {
            let _ = handle.await;
        }
    });

    // Feed details to writer
    for handle in detail_handles {
        match handle.await {
            Ok((slug, result)) => {
                if detail_tx.send(DetailResult { slug, detail: result }).await.is_err() {
                    break;
                }
            }
            Err(_) => {
                if detail_tx
                    .send(DetailResult {
                        slug: "task-error".into(),
                        detail: Err(anyhow::anyhow!("Task error")),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    drop(detail_tx);

    // Wait for writer to finish, print progress in background
    let progress_total = total;
    let progress_start = start_time;
    let progress_fetcher = Arc::clone(&fetcher);
    let progress_success = Arc::clone(&success);
    let progress_failed = Arc::clone(&failed);
    let progress_ch = Arc::clone(&total_chapters);
    let progress_img = Arc::clone(&total_ch_images);

    // Progress reporter
    let progress_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let s = progress_success.load(Ordering::Relaxed);
            let f = progress_failed.load(Ordering::Relaxed);
            let c = s + f;
            if c == 0 {
                continue;
            }
            let elapsed = progress_start.elapsed().as_secs_f64();
            let rate = c as f64 / elapsed * 60.0;
            let eta = if rate > 0.0 {
                (progress_total - c) as f64 / rate
            } else {
                0.0
            };
            let stats = progress_fetcher.stats();
            eprint!(
                "  [{}/{} {:.1}%] {:.0} komik/min | ETA: {:.0}min | \
                 OK: {} FAIL: {} | Ch: {} Img: {} | DL: {:.1}MB | req: {}\r",
                c,
                progress_total,
                c as f64 / progress_total as f64 * 100.0,
                rate,
                eta,
                s,
                f,
                progress_ch.load(Ordering::Relaxed),
                progress_img.load(Ordering::Relaxed),
                stats.mb_downloaded(),
                stats.requests,
            );
        }
    });

    // Wait for writer
    let _ = writer_task.await;
    progress_handle.abort();

    // Final stats
    let elapsed = start_time.elapsed().as_secs_f64();
    let stats = fetcher.stats();
    let s = success.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let ch = total_chapters.load(Ordering::Relaxed);
    let img = total_ch_images.load(Ordering::Relaxed);

    let jsonl_size_mb = std::fs::metadata(jsonl_path.as_ref())
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    println!();
    println!("{}", "=".repeat(70));
    println!("  FULL FETCH COMPLETE");
    println!("  Total komik:    {total}");
    println!("  Success:        {s}");
    println!("  Failed:         {f}");
    println!("  Chapters:       {ch}");
    println!("  Images:         {img}");
    println!("  Time:           {:.1}s ({:.1}min)", elapsed, elapsed / 60.0);
    println!(
        "  Rate:           {:.1} komik/min",
        (s + f) as f64 / elapsed * 60.0
    );
    println!("  Requests:       {}", stats.requests);
    println!("  Retries:        {}", stats.retries);
    println!("  Downloaded:     {:.1} MB", stats.mb_downloaded());
    if elapsed > 0.0 {
        println!(
            "  Bandwidth:      {:.1} MB/s",
            stats.mb_downloaded() / elapsed
        );
    }
    println!(
        "  JSONL:          {} ({:.1} MB)",
        jsonl_path.display(),
        jsonl_size_mb
    );
    println!("{}", "=".repeat(70));

    Ok(())
}

// ============================================================
// SMART UPDATE (incremental dari /komik-terbaru/)
// ============================================================
//
// Design:
//   1. Load DB ke HashMap<slug, KomikDetail> (single lookup O(1))
//   2. Fetch /komik-terbaru/ dengan pagination otomatis
//   3. Single pass: compare setiap item dengan DB
//      - Slug baru → scrape detail (1 request per komik baru)
//      - Chapter update → construct URL (0 request!)
//      - Sama → skip
//   4. Save DB (full rewrite)
//
// Efisiensi:
//   - Update 6 jam: ~40-80 item dari terbaru
//   - Dari itu: ~2-5 komik baru (scrape detail)
//   - Sisanya: cuma increment chapter number (0 request)
//   - Total requests per update: 2-5 (bukan 8701!)

struct UpdateOpts {
    max_pages: u32,
    max_age_minutes: u32,
    timeout: u64,
    proxy: Option<String>,
    dry_run: bool,
}

async fn run_update(opts: UpdateOpts) -> Result<()> {
    let start_time = Instant::now();

    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("komik_db.jsonl");

    println!("{}", "=".repeat(70));
    println!("  KOMIKINDO SMART UPDATE");
    println!("  Max pages: {} | Max age: {}min", opts.max_pages, opts.max_age_minutes);
    if opts.dry_run {
        println!("  *** DRY RUN - no changes will be saved ***");
    }
    println!("{}", "=".repeat(70));

    // === Step 1: Load existing DB ===
    println!("\n--- Step 1: Loading DB ---");
    let mut db = jsonl::load_db(&db_path);
    let db_count_before = db.len();
    if db_count_before > 0 {
        println!("[DB] Loaded {} komik from {}", db_count_before, db_path.display());
    } else {
        println!("[DB] No existing DB found at {} — starting fresh", db_path.display());
    }

    let fetcher = Arc::new(Fetcher::new(opts.timeout, opts.proxy.as_deref())?);

    // === Step 2: Fetch /komik-terbaru/ ===
    println!("\n--- Step 2: Fetching /komik-terbaru/ ---");
    let terbaru_items = scrape_komik_terbaru(&fetcher, opts.max_pages, opts.max_age_minutes).await?;
    if terbaru_items.is_empty() {
        println!("[TERBARU] No items found. Nothing to update.");
        return Ok(());
    }
    println!("[TERBARU] {} items fetched", terbaru_items.len());

    // === Step 3: Single-pass compare ===
    println!("\n--- Step 3: Comparing with DB ---");
    let mut new_komiks: Vec<parsers::TerbaruItem> = Vec::new();
    let mut updated_chapters: Vec<(String, f64, f64, i64)> = Vec::new(); // (slug, old_ch, new_ch, gap)
    let mut skipped = 0usize;

    for item in &terbaru_items {
        if let Some(existing) = db.get_mut(&item.slug) {
            // Slug ADA di DB → bandingkan chapter
            let stored_ch = existing.latest_chapter_number.unwrap_or(0.0);

            if item.chapter_number <= stored_ch {
                // Sama atau terbaru lebih kecil → skip
                skipped += 1;
            } else {
                // BEDA → construct chapter URLs (0 request!)
                let old_ch = stored_ch;
                let new_ch = item.chapter_number;
                let gap = new_ch as i64 - old_ch as i64;

                // Construct URLs untuk chapter baru
                for ch_num in (old_ch as i64 + 1)..=(new_ch as i64) {
                    let ch_url = parsers::construct_chapter_url(&item.url_base, ch_num as f64);
                    existing.chapters.push(parsers::ChapterInfo {
                        number: ch_num as f64,
                        url: ch_url,
                        cdn_domain_id: None,
                        cdn_path_prefix: None,
                        image_filenames: None,
                        image_ext_ids: None,
                        total_images: None,
                    });
                }

                // Re-sort chapters descending
                existing.chapters.sort_by(|a, b| {
                    b.number
                        .partial_cmp(&a.number)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                existing.latest_chapter_number = Some(new_ch);

                updated_chapters.push((item.slug.clone(), old_ch, new_ch, gap));
            }
        } else {
            // Slug TIDAK ADA → komik baru!
            new_komiks.push(item.clone());
        }
    }

    println!("[COMPARE] New komiks: {}", new_komiks.len());
    println!("[COMPARE] Updated chapters: {}", updated_chapters.len());
    println!("[COMPARE] Skipped (unchanged): {}", skipped);

    // Print chapter updates
    for (slug, old_ch, new_ch, gap) in &updated_chapters {
        println!("  + {}: ch.{:.0} → ch.{:.0} (+{} chapters, 0 requests)", slug, old_ch, new_ch, gap);
    }

    // Print new komiks
    for item in &new_komiks {
        println!("  * {} (ch.{:.0}) — will scrape detail", item.judul, item.chapter_number);
    }

    // === Step 4: Scrape detail untuk komik BARU ===
    if !new_komiks.is_empty() {
        println!("\n--- Step 4: Scraping {} new komik details ---", new_komiks.len());

        let mut detail_handles = Vec::with_capacity(new_komiks.len());
        for item in &new_komiks {
            let fetcher = Arc::clone(&fetcher);
            let slug = item.slug.clone();
            detail_handles.push(tokio::spawn(async move {
                let result = scrape_komik_detail(&slug, &fetcher).await;
                (slug, result)
            }));
        }

        let mut new_success = 0usize;
        let mut new_failed = 0usize;

        for handle in detail_handles {
            match handle.await {
                Ok((slug, Ok(detail))) => {
                    println!("[NEW] + {} — {}", slug, detail.judul.as_deref().unwrap_or("?"));
                    new_success += 1;
                    db.insert(slug, detail);
                }
                Ok((slug, Err(e))) => {
                    eprintln!("[NEW FAIL] {} — {}", slug, e);
                    new_failed += 1;
                }
                Err(e) => {
                    eprintln!("[NEW FAIL] task error: {}", e);
                    new_failed += 1;
                }
            }
        }

        println!("[NEW] {} success, {} failed", new_success, new_failed);
    }

    // === Step 5: Save DB ===
    if opts.dry_run {
        println!("\n--- DRY RUN: skipping save ---");
    } else {
        println!("\n--- Step 5: Saving DB ---");
        jsonl::save_db(&db_path, &db)?;

        let db_size = std::fs::metadata(&db_path)
            .map(|m| m.len() as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0);

        println!("[DB] Saved {} komik to {} ({:.1} MB)", db.len(), db_path.display(), db_size);
    }

    // === Summary ===
    let elapsed = start_time.elapsed();
    let stats = fetcher.stats();
    let new_count = db.len() - db_count_before;

    println!();
    println!("{}", "=".repeat(70));
    println!("  UPDATE COMPLETE");
    println!("  DB before:     {} komik", db_count_before);
    println!("  DB after:      {} komik (+{} new)", db.len(), new_count);
    println!("  New komiks:    {} ({} detail fetches)", new_komiks.len(), new_komiks.len());
    println!("  Updated ch:    {} (0 extra requests!)", updated_chapters.len());
    println!("  Skipped:       {}", skipped);
    println!("  Total requests:{}", stats.requests);
    println!("  Time:          {:.1}s", elapsed.as_secs_f64());
    println!("  Saved:         {}", if opts.dry_run { "NO (dry run)" } else { "YES" });
    println!("{}", "=".repeat(70));

    Ok(())
}

// ============================================================
// HELPERS
// ============================================================

struct DetailResult {
    slug: String,
    detail: Result<KomikDetail>,
}

fn create_jsonl_path(data_dir: &std::path::Path, dt: &DateTime<Local>) -> PathBuf {
    let timestamp = dt.format("%Y%m%d_%H%M%S").to_string();
    data_dir.join(format!("full_fetch_{timestamp}.jsonl"))
}
