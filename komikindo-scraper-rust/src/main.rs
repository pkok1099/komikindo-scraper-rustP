/// KomikIndo Scraper - Rust Version (FULL SPEED + SMART UPDATE + DB)
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
/// DB MODE (Method 2 - tanpa image):
///   - Set DATABASE_URL di .env untuk aktifkan
///   - Upsert komik metadata + chapters + genres ke Supabase PostgreSQL
///   - Tanpa image scraping (chapters table: hanya number, tanpa filenames)
///   - Jika DATABASE_URL kosong, fallback ke JSONL file

mod config;
mod db;
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
use sqlx::PgPool;

use crate::fetcher::Fetcher;
use crate::scraper::{scrape_full_komik_list, scrape_komik_detail, scrape_komik_terbaru};

// ============================================================
// CLI
// ============================================================

#[derive(Parser, Debug)]
#[command(name = "komikindo-scraper")]
#[command(about = "KomikIndo Scraper - Rust Full Speed + Smart Update + DB")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Full fetch semua komik + detail (FULL SPEED)
    /// Method 2: tanpa image scraping.
    FullFetch {
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

        /// Force write ke database (requires DATABASE_URL)
        #[arg(long)]
        db: bool,
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

        /// Force write ke database (requires DATABASE_URL)
        #[arg(long)]
        db: bool,
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(8192)
        .build()?;

    runtime.block_on(async move {
        match cli.command {
            Commands::FullFetch {
                limit,
                start_from,
                resume,
                timeout,
                proxy,
                db,
            } => {
                run_full_fetch(FullFetchOpts {
                    limit,
                    start_from,
                    resume,
                    timeout,
                    proxy,
                    use_db: db,
                })
                .await?;
            }
            Commands::Update {
                max_pages,
                max_age_minutes,
                timeout,
                proxy,
                dry_run,
                db,
            } => {
                run_update(UpdateOpts {
                    max_pages,
                    max_age_minutes,
                    timeout,
                    proxy,
                    dry_run,
                    use_db: db,
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
// DB CONNECTION HELPER
// ============================================================

/// Connect ke DB jika DATABASE_URL tersedia.
/// Returns None jika tidak ada DATABASE_URL.
async fn maybe_connect_db(use_db: bool) -> Option<(PgPool, bool)> {
    let cfg = config::env_config();

    // Cek apakah DATABASE_URL tersedia
    let has_db_url = !cfg.database_url.is_empty();
    let should_use_db = use_db || has_db_url;

    if !should_use_db {
        return None;
    }

    if !has_db_url {
        eprintln!("[DB] --db flag di-set tapi DATABASE_URL tidak ditemukan di .env");
        eprintln!("[DB] Falling back to JSONL mode");
        return None;
    }

    println!("[DB] Connecting to Supabase PostgreSQL...");
    match db::connect(&cfg.database_url).await {
        Ok(pool) => {
            println!("[DB] Connected!");
            Some((pool, true))
        }
        Err(e) => {
            eprintln!("[DB] Connection failed: {e}");
            eprintln!("[DB] Falling back to JSONL mode");
            None
        }
    }
}

// ============================================================
// FULL FETCH (Method 2: tanpa image, dengan optional DB)
// ============================================================
//
// Design:
//   1. Fetch detail untuk SEMUA komik sekaligus (8671 spawn_blocking)
//   2. Setiap detail selesai → write ke DB (atau JSONL)
//   3. Tidak ada image scraping (Method 2)
//   4. Bottleneck = internet saja

struct FullFetchOpts {
    limit: usize,
    start_from: Option<String>,
    resume: bool,
    timeout: u64,
    proxy: Option<String>,
    use_db: bool,
}

async fn run_full_fetch(opts: FullFetchOpts) -> Result<()> {
    let start_time = Instant::now();
    let dt_start = Local::now();

    // DB connection (optional)
    let db_pool = maybe_connect_db(opts.use_db).await;
    let using_db = db_pool.is_some();

    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;

    // JSONL path (selalu dibuat sebagai backup)
    let mut resume_slug = opts.start_from.unwrap_or_default();

    let jsonl_path = if opts.resume && !using_db {
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
    } else if !using_db {
        create_jsonl_path(&data_dir, &dt_start)
    } else {
        // DB mode: tetap buat JSONL sebagai backup
        create_jsonl_path(&data_dir, &dt_start)
    };

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
    if using_db {
        println!("  KOMIKINDO FULL FETCH → Supabase DB");
    } else {
        println!("  KOMIKINDO FULL FETCH → JSONL");
    }
    println!("  Started at {}", dt_start.format("%Y-%m-%d %H:%M:%S"));
    println!("  Timeout: {}s", opts.timeout);
    println!("  Mode: Method 2 (tanpa image)");
    if using_db {
        println!("  Storage: Supabase PostgreSQL + JSONL backup");
    } else {
        println!("  Storage: JSONL file");
    }
    println!("  Blocking threads: 8192 (1 per curl request)");
    println!("{proxy_info}");
    println!("{}", "=".repeat(70));

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
    println!("\n--- Step 2: FULL SPEED PIPELINE ---");
    println!("[INFO] Spawning {} detail fetches (no chapter scraping)", total);

    let success = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let total_chapters = Arc::new(AtomicUsize::new(0));
    let jsonl_path = Arc::new(jsonl_path);
    let db_new = Arc::new(AtomicUsize::new(0));
    let db_updated = Arc::new(AtomicUsize::new(0));

    // Spawn ALL detail fetches at once
    let mut detail_handles = Vec::with_capacity(total);
    for slug in &komik_list {
        let fetcher = Arc::clone(&fetcher);
        let slug = slug.clone();
        detail_handles.push(tokio::spawn(async move {
            let result = scrape_komik_detail(&slug, &fetcher).await;
            (slug, result)
        }));
    }

    // Process results as they arrive
    let write_success = Arc::clone(&success);
    let write_failed = Arc::clone(&failed);
    let write_ch = Arc::clone(&total_chapters);
    let write_jsonl = Arc::clone(&jsonl_path);
    let db_pool_ref = db_pool.as_ref().map(|(p, _)| p.clone());
    let write_db_new = Arc::clone(&db_new);
    let write_db_updated = Arc::clone(&db_updated);

    let writer_task = tokio::spawn(async move {
        for handle in detail_handles {
            match handle.await {
                Ok((slug, result)) => {
                    match result {
                        Ok(detail) => {
                            let ch_count = detail.chapters.len();
                            write_ch.fetch_add(ch_count, Ordering::Relaxed);

                            // Write to DB if connected
                            if let Some(ref pool) = db_pool_ref {
                                match db::write_komik(pool, &detail).await {
                                    Ok(wr) => {
                                        if wr.is_new {
                                            write_db_new.fetch_add(1, Ordering::Relaxed);
                                        } else {
                                            write_db_updated.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                    Err(e) => {
                                        if let Some(db_err) = e.downcast_ref::<sqlx::Error>() {
                                            eprintln!("  [DB ERR] {}: {} | {:?}", slug, e, db_err);
                                        } else {
                                            eprintln!("  [DB ERR] {}: {:#}", slug, e);
                                        }
                                    }
                                }
                            }

                            // Always write JSONL as backup
                            if let Err(e) = jsonl::append_jsonl(&write_jsonl, &detail) {
                                eprintln!("  [WARN] JSONL write error: {e}");
                            }

                            write_success.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            write_failed.fetch_add(1, Ordering::Relaxed);
                            // Write error to JSONL
                            let failed_json = serde_json::json!({
                                "slug": slug,
                                "_status": "detail_failed",
                                "_error": e.to_string(),
                            });
                            let _ = jsonl::append_jsonl_raw(&write_jsonl, &failed_json.to_string());
                        }
                    }
                }
                Err(_) => {
                    write_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });

    // Progress reporter
    let progress_total = total;
    let progress_start = start_time;
    let progress_fetcher = Arc::clone(&fetcher);
    let progress_success = Arc::clone(&success);
    let progress_failed = Arc::clone(&failed);
    let progress_ch = Arc::clone(&total_chapters);
    let progress_db_new = Arc::clone(&db_new);
    let progress_db_updated = Arc::clone(&db_updated);

    let progress_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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

            if using_db {
                eprint!(
                    "  [{}/{} {:.1}%] {:.0} komik/min | ETA: {:.0}min | \
                     OK: {} FAIL: {} | DB new: {} upd: {} | Ch: {} | DL: {:.1}MB | req: {}\r",
                    c, progress_total, c as f64 / progress_total as f64 * 100.0,
                    rate, eta, s, f,
                    progress_db_new.load(Ordering::Relaxed),
                    progress_db_updated.load(Ordering::Relaxed),
                    progress_ch.load(Ordering::Relaxed),
                    stats.mb_downloaded(), stats.requests,
                );
            } else {
                eprint!(
                    "  [{}/{} {:.1}%] {:.0} komik/min | ETA: {:.0}min | \
                     OK: {} FAIL: {} | Ch: {} | DL: {:.1}MB | req: {}\r",
                    c, progress_total, c as f64 / progress_total as f64 * 100.0,
                    rate, eta, s, f,
                    progress_ch.load(Ordering::Relaxed),
                    stats.mb_downloaded(), stats.requests,
                );
            }
        }
    });

    // Wait for writer
    let _ = writer_task.await;
    progress_handle.abort();

    // === Summary ===
    let elapsed = start_time.elapsed().as_secs_f64();
    let stats = fetcher.stats();
    let s = success.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let ch = total_chapters.load(Ordering::Relaxed);
    let db_n = db_new.load(Ordering::Relaxed);
    let db_u = db_updated.load(Ordering::Relaxed);

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
    if using_db {
        println!("  DB new:         {db_n}");
        println!("  DB updated:     {db_u}");
    }
    println!("  Time:           {:.1}s ({:.1}min)", elapsed, elapsed / 60.0);
    println!(
        "  Rate:           {:.1} komik/min",
        (s + f) as f64 / elapsed * 60.0
    );
    println!("  Requests:       {}", stats.requests);
    println!("  Retries:        {}", stats.retries);
    println!("  Downloaded:     {:.1} MB", stats.mb_downloaded());
    println!(
        "  JSONL:          {} ({:.1} MB)",
        jsonl_path.display(),
        jsonl_size_mb
    );

    // Log ke scrape_log
    if let Some((pool, _)) = &db_pool {
        let _ = db::log_scrape(
            pool, "full_fetch", "completed",
            total as i32, db_n as i32, db_u as i32, f as i32,
            Some(&format!("{:.1}s, {} requests", elapsed, stats.requests)),
        ).await;
    }

    println!("{}", "=".repeat(70));

    Ok(())
}

// ============================================================
// SMART UPDATE (incremental dari /komik-terbaru/)
// ============================================================

struct UpdateOpts {
    max_pages: u32,
    max_age_minutes: u32,
    timeout: u64,
    proxy: Option<String>,
    dry_run: bool,
    use_db: bool,
}

async fn run_update(opts: UpdateOpts) -> Result<()> {
    let start_time = Instant::now();

    // DB connection (optional)
    let db_pool = maybe_connect_db(opts.use_db).await;
    let using_db = db_pool.is_some();

    println!("{}", "=".repeat(70));
    println!("  KOMIKINDO SMART UPDATE");
    println!("  Max pages: {} | Max age: {}min", opts.max_pages, opts.max_age_minutes);
    if opts.dry_run {
        println!("  *** DRY RUN - no changes will be saved ***");
    }
    if using_db {
        println!("  Storage: Supabase PostgreSQL");
    } else {
        println!("  Storage: JSONL file (fallback)");
    }
    println!("{}", "=".repeat(70));

    // === Step 1: Load existing data ===
    println!("\n--- Step 1: Loading existing data ---");

    // DB chapter map: slug -> (komik_id, latest_chapter_number)
    let mut db_chapter_map: HashMap<String, (i32, Option<f64>)> = HashMap::new();
    if let Some((pool, _)) = &db_pool {
        match db::load_chapter_map(pool).await {
            Ok(map) => {
                println!("[DB] Loaded {} komik from Supabase", map.len());
                db_chapter_map = map;
            }
            Err(e) => {
                eprintln!("[DB] Failed to load from Supabase: {e}");
            }
        }
    }

    // Fallback: load JSONL if DB is empty or unavailable
    let mut jsonl_db = HashMap::new();
    if db_chapter_map.is_empty() {
        let data_dir = PathBuf::from("data");
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("komik_db.jsonl");
        jsonl_db = jsonl::load_db(&db_path);
        if !jsonl_db.is_empty() {
            println!("[JSONL] Loaded {} komik from {}", jsonl_db.len(), db_path.display());
        }
    }

    let total_before = db_chapter_map.len().max(jsonl_db.len());
    if total_before > 0 {
        println!("[DATA] Total existing komik: {total_before}");
    } else {
        println!("[DATA] No existing data — starting fresh");
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
    println!("\n--- Step 3: Comparing with existing data ---");
    let mut new_komiks: Vec<parsers::TerbaruItem> = Vec::new();
    let mut updated_chapters: Vec<(String, f64, f64, i64)> = Vec::new();
    let mut skipped = 0usize;

    for item in &terbaru_items {
        // Cek di DB dulu, lalu JSONL fallback
        let stored_ch = if let Some((_, latest)) = db_chapter_map.get(&item.slug) {
            *latest
        } else if let Some(existing) = jsonl_db.get(&item.slug) {
            existing.latest_chapter_number
        } else {
            None
        };

        if let Some(stored_ch) = stored_ch {
            if item.chapter_number <= stored_ch {
                skipped += 1;
            } else {
                let old_ch = stored_ch;
                let new_ch = item.chapter_number;
                let gap = new_ch as i64 - old_ch as i64;
                updated_chapters.push((item.slug.clone(), old_ch, new_ch, gap));
            }
        } else {
            new_komiks.push(item.clone());
        }
    }

    println!("[COMPARE] New komiks: {}", new_komiks.len());
    println!("[COMPARE] Updated chapters: {}", updated_chapters.len());
    println!("[COMPARE] Skipped (unchanged): {}", skipped);

    for (slug, old_ch, new_ch, gap) in &updated_chapters {
        println!("  + {}: ch.{:.0} → ch.{:.0} (+{} chapters, 0 requests)", slug, old_ch, new_ch, gap);
    }

    for item in &new_komiks {
        println!("  * {} (ch.{:.0}) — will scrape detail", item.judul, item.chapter_number);
    }

    // === Step 4: Scrape detail untuk komik BARU + write ke DB/JSONL ===
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

                    // Write to DB
                    if let Some((pool, _)) = &db_pool {
                        if !opts.dry_run {
                            match db::write_komik(pool, &detail).await {
                                Ok(wr) => println!("  [DB] id={}, chapters={}, genres={} ({})",
                                    wr.komik_id, wr.chapters_inserted, wr.genres_synced,
                                    if wr.is_new { "NEW" } else { "UPDATED" }),
                                Err(e) => {
                                    if let Some(db_err) = e.downcast_ref::<sqlx::Error>() {
                                        eprintln!("  [DB ERR] {}: {} | {:?}", slug, e, db_err);
                                    } else {
                                        eprintln!("  [DB ERR] {}: {:#}", slug, e);
                                    }
                                }
                            }
                        }
                    }

                    // Also update JSONL
                    jsonl_db.insert(slug.clone(), detail);
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

    // === Step 5: Save ===
    if opts.dry_run {
        println!("\n--- DRY RUN: skipping save ---");
    } else {
        println!("\n--- Step 5: Saving ---");

        // Save JSONL backup
        if !jsonl_db.is_empty() {
            let data_dir = PathBuf::from("data");
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("komik_db.jsonl");
            jsonl::save_db(&db_path, &jsonl_db)?;

            let db_size = std::fs::metadata(&db_path)
                .map(|m| m.len() as f64 / 1024.0 / 1024.0)
                .unwrap_or(0.0);
            println!("[JSONL] Saved {} komik to {} ({:.1} MB)", jsonl_db.len(), db_path.display(), db_size);
        }

        // Log ke scrape_log
        if let Some((pool, _)) = &db_pool {
            let _ = db::log_scrape(
                pool, "smart_update", "completed",
                new_komiks.len() as i32,
                new_komiks.len() as i32,
                updated_chapters.len() as i32,
                0,
                Some(&format!(
                    "pages={}, max_age={}min, skipped={}",
                    opts.max_pages, opts.max_age_minutes, skipped
                )),
            ).await;
            println!("[DB] Scrape logged to Supabase");
        }
    }

    // === Summary ===
    let elapsed = start_time.elapsed();
    let stats = fetcher.stats();

    println!();
    println!("{}", "=".repeat(70));
    println!("  UPDATE COMPLETE");
    println!("  Data before:    {} komik", total_before);
    println!("  New komiks:     {} ({} detail fetches)", new_komiks.len(), new_komiks.len());
    println!("  Updated ch:     {} (0 extra requests!)", updated_chapters.len());
    println!("  Skipped:        {}", skipped);
    println!("  Total requests: {}", stats.requests);
    println!("  Time:           {:.1}s", elapsed.as_secs_f64());
    println!("  Saved:          {}", if opts.dry_run { "NO (dry run)" } else { "YES" });
    println!("{}", "=".repeat(70));

    Ok(())
}

// ============================================================
// HELPERS
// ============================================================

fn create_jsonl_path(data_dir: &std::path::Path, dt: &DateTime<Local>) -> PathBuf {
    let timestamp = dt.format("%Y%m%d_%H%M%S").to_string();
    data_dir.join(format!("full_fetch_{timestamp}.jsonl"))
}
