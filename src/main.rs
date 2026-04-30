/// KomikIndo Scraper - Rust Version (FULL SPEED + SMART UPDATE + DB)
///
/// FULL SPEED DESIGN:
///   - Tidak ada semaphore / concurrency limit
///   - Bottleneck hanya di internet (bandwidth + latency)
///   - Streaming pipeline: detail selesai → chapter langsung jalan
///   - Sekarang ada limit: --max-blocking-threads dan --max-in-flight
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
///
/// WORKFLOW:
///   1. full-fetch  → Fetch semua komik ke JSONL (TANPA DB write, ringan)
///   2. upload-db   → Upload JSONL ke DB (manual, batch insert)
///   3. update      → Smart update otomatis (write ke DB langsung)
///
/// DEBUG FEATURES:
///   - `--verbose` / `-v` global flag: curl protocol details, timing, response info
///   - `--env /path/to/.env` global flag: explicit .env file location
///   - `debug` subcommand: env diagnostics, DB test, network test, binary info

// All modules declared in lib.rs — re-import for convenience
use komikindo_scraper::{config, db, fetcher, jsonl, parsers, scraper};

// jemalloc: better allocation performance for allocation-heavy workloads (5-15% improvement).
// Only linked on Linux/macOS (not Android/Termux or Windows).
#[cfg(all(unix, not(target_os = "android")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use sqlx::PgPool;
use sqlx::Row;

use config::BASE_URL;
use fetcher::Fetcher;
use parsers::KomikDetail;
use scraper::{scrape_chapter_images, scrape_full_komik_list, scrape_komik_detail, scrape_komik_terbaru};

// ============================================================
// CLI
// ============================================================

#[derive(Parser, Debug)]
#[command(name = "komikindo-scraper")]
#[command(about = "KomikIndo Scraper - Rust Full Speed + Smart Update + DB")]
#[command(version)]
#[command(after_help = r#"ENVIRONMENT:
  DATABASE_URL    PostgreSQL connection string (Supabase)
  PROXY_URL       Default proxy URL (socks5://host:port)
  PROXY_ENABLED   Set to "1" to enable default proxy
  SCRAPER_RETRIES Max retry attempts (default: 3)
  SCRAPER_TIMEOUT Request timeout in seconds (default: 30)

EXAMPLES:
  komikindo-scraper check                              # Test connectivity
  komikindo-scraper -v check --proxy socks5://...       # Verbose with proxy
  komikindo-scraper update --dry-run                    # Preview changes
  komikindo-scraper update --db                         # Update DB
  komikindo-scraper full-fetch --limit 10              # Fetch 10 komik ke JSONL
  komikindo-scraper upload-db                            # Upload JSONL terbaru ke DB
  komikindo-scraper upload-db --file data/full_fetch_xxx.jsonl  # Upload file tertentu
  komikindo-scraper --env /path/to/.env update --db     # Specify .env path
  komikindo-scraper debug                               # Run diagnostics
"#)]
struct Cli {
    /// Enable verbose output (curl protocol details, timing, response info)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Specify .env file path (searches: ./, binary dir, parent dirs if not set)
    #[arg(long, global = true)]
    env: Option<String>,

    /// Tokio worker threads (CPU). Default = Tokio default.
    #[arg(long, global = true)]
    worker_threads: Option<usize>,

    /// Max blocking threads for libcurl spawn_blocking (default: 256).
    /// Set equal to max-in-flight for optimal connection reuse.
    /// Lower than 512 to prevent curl error 2 (Failed initialization)
    /// when the blocking thread pool is exhausted under high concurrency.
    #[arg(long, global = true, default_value_t = 256)]
    max_blocking_threads: usize,

    /// Max in-flight HTTP requests (default: 200).
    /// 200 is optimal: avoids curl error 2 (blocking thread pool exhaustion)
    /// while still achieving 60K+ komik/min throughput.
    #[arg(long, global = true, default_value_t = 200)]
    max_in_flight: usize,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Full fetch semua komik + detail (FULL SPEED)
    /// Fetch semua data ke JSONL file, TANPA auto upload ke DB.
    /// Gunakan command 'upload-db' untuk upload ke database.
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

        /// SOCKS5/HTTP proxy URL (e.g. socks5://127.0.0.1:1080)
        #[arg(long)]
        proxy: Option<String>,

        /// Automatically upload to DB after fetch completes
        #[arg(long)]
        auto_upload: bool,

        /// Fetch chapter images (CDN URLs) untuk setiap chapter.
        /// Ini akan fetch setiap halaman chapter, jadi total request = jumlah chapter.
        /// Signifikan lebih lambat tapi data lengkap dengan image URLs.
        #[arg(long)]
        with_images: bool,
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

        /// SOCKS5/HTTP proxy URL (e.g. socks5://127.0.0.1:1080)
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

    /// Test connectivity to komikindo.ch (with optional proxy)
    /// Use --verbose to see full curl protocol details
    Check {
        /// SOCKS5/HTTP proxy URL (e.g. socks5://127.0.0.1:1080)
        #[arg(long)]
        proxy: Option<String>,

        /// Timeout per request dalam detik
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },

    /// Run diagnostics: env config, .env loading, DB connection, network test, binary info
    Debug {
        /// Run all tests (env + db + network + info)
        #[arg(long, default_value_t = false)]
        all: bool,

        /// Test DB connection only
        #[arg(long)]
        db: bool,

        /// Test network only (direct + env proxy)
        #[arg(long)]
        network: bool,

        /// Show environment/config info only
        #[arg(long)]
        show_env: bool,

        /// Show binary/platform info only
        #[arg(long)]
        info: bool,

        /// SOCKS5/HTTP proxy URL for network test (overrides PROXY_URL)
        #[arg(long)]
        proxy: Option<String>,

        /// Timeout per request dalam detik
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },

    /// Benchmark parsing speed (fetch once, parse N times)
    BenchParse {
        /// What to benchmark
        #[arg(long, value_parser = ["list", "homepage", "terbaru", "detail"])]
        kind: String,

        /// Iterations (parse repeats)
        #[arg(long, default_value_t = 200)]
        iters: u32,

        /// Slug for --kind detail
        #[arg(long)]
        slug: Option<String>,

        /// Timeout per request dalam detik
        #[arg(long, default_value_t = 30)]
        timeout: u64,

        /// SOCKS5/HTTP proxy URL (e.g. socks5://127.0.0.1:1080)
        #[arg(long)]
        proxy: Option<String>,
    },

    /// Query DB and show saved data (readback verification)
    DbShow {
        /// Limit rows to show (default 5)
        #[arg(long, default_value_t = 5)]
        limit: i64,

        /// Show specific komik by slug (if set, ignores --limit for komik list)
        #[arg(long)]
        slug: Option<String>,

        /// Also show last N chapters per komik (default 5)
        #[arg(long, default_value_t = 5)]
        chapters: i64,
    },

    /// Print full komik detail from DB (JSON) by slug
    DbDetail {
        #[arg(long)]
        slug: String,

        /// Limit number of chapters returned (default 200)
        #[arg(long, default_value_t = 200)]
        chapters: i64,
    },

    /// Inspect DB schema (tables/columns) without psql
    DbSchema {
        /// Which table to inspect (komik, chapters, komik_genres, scrape_log)
        #[arg(long, default_value = "chapters")]
        table: String,
    },

    /// Reset DB data (alpha/testing): TRUNCATE main tables
    DbReset {
        /// Also reset identity/serial counters
        #[arg(long, default_value_t = true)]
        restart_identity: bool,
    },

    /// Setup DB schema (create tables/columns/indexes if missing)
    DbSetup,

    /// Upload JSONL data ke database (manual batch insert)
    /// Gunakan setelah full-fetch selesai.
    UploadDb {
        /// Path ke file JSONL (default: terbaru di data/)
        #[arg(long)]
        file: Option<String>,

        /// Batch size untuk DB insert (default: 1000)
        #[arg(long, default_value_t = 1000)]
        batch_size: usize,
    },

    /// Drop all scraper tables (DANGEROUS): removes tables completely
    DbDropAll,

    /// [EXPERIMENTAL] LM-based title detection using ONNX model
    /// Compare LM-detected title vs hardcoded selector title.
    LmDetect {
        /// Limit jumlah komik untuk test (0 = semua di JSONL terbaru)
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Field yang dideteksi: title, rating, atau all
        #[arg(long, default_value = "all")]
        field: String,
    },
}

// ============================================================
// MAIN (bounded concurrency)
// ============================================================

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set explicit .env path BEFORE any access to env_config()
    config::set_explicit_env_path(cli.env.clone());

    // Set log level based on verbose flag
    let log_level = if cli.verbose { "debug" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .init();

    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if let Some(n) = cli.worker_threads {
        rt_builder.worker_threads(n.max(1));
    }
    rt_builder.max_blocking_threads(cli.max_blocking_threads.max(1));
    let runtime = rt_builder.build()?;

    let verbose = cli.verbose;
    let max_in_flight = cli.max_in_flight;

    runtime.block_on(async move {
        match cli.command {
            Commands::FullFetch {
                limit,
                start_from,
                resume,
                timeout,
                proxy,
                auto_upload,
                with_images,
            } => {
                run_full_fetch(FullFetchOpts {
                    limit,
                    start_from,
                    resume,
                    timeout,
                    proxy,
                    verbose,
                    max_in_flight,
                    auto_upload,
                    with_images,
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
                    verbose,
                    max_in_flight,
                })
                .await?;
            }
            Commands::Homepage { proxy } => {
                let fetcher = Fetcher::new(30, proxy.as_deref(), verbose, max_in_flight)?;
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
            Commands::Check { proxy, timeout } => {
                run_check(&proxy, timeout, verbose, max_in_flight).await?;
            }
            Commands::Debug { all, db, network, show_env, info, proxy, timeout } => {
                run_debug(DebugOpts {
                    all,
                    db,
                    network,
                    show_env,
                    info,
                    proxy,
                    timeout,
                    verbose,
                    max_in_flight,
                })
                .await?;
            }
            Commands::BenchParse { kind, iters, slug, timeout, proxy } => {
                run_bench_parse(kind, iters, slug, timeout, proxy, verbose, max_in_flight).await?;
            }
            Commands::DbShow { limit, slug, chapters } => {
                run_db_show(limit, slug, chapters).await?;
            }
            Commands::DbDetail { slug, chapters } => {
                run_db_detail(slug, chapters).await?;
            }
            Commands::DbSchema { table } => {
                run_db_schema(table).await?;
            }
            Commands::DbReset { restart_identity } => {
                run_db_reset(restart_identity).await?;
            }
            Commands::DbSetup => {
                run_db_setup().await?;
            }
            Commands::DbDropAll => {
                run_db_drop_all().await?;
            }
            Commands::UploadDb { file, batch_size } => {
                run_upload_db(file, batch_size).await?;
            }
            Commands::LmDetect { limit, field } => {
                run_lm_detect(limit, field.clone()).await?;
            }
        }
        Ok(())
    })
}

async fn run_db_show(limit: i64, slug: Option<String>, chapters: i64) -> Result<()> {
    let cfg = config::env_config();
    if cfg.database_url.is_empty() {
        anyhow::bail!("DATABASE_URL kosong. Set DATABASE_URL di .env lalu coba lagi.");
    }
    let pool = db::connect(&cfg.database_url).await?;

    println!("{}", "=".repeat(60));
    println!("  DB READBACK");
    println!("{}", "=".repeat(60));

    if let Some(slug) = slug {
        let Some(komik_id) = db::get_komik_id_by_slug(&pool, &slug).await? else {
            println!("[DB] slug not found: {slug}");
            return Ok(());
        };
        let ch = db::list_chapters(&pool, komik_id, chapters).await?;
        println!("[DB] slug={slug} id={komik_id} chapters_shown={}", ch.len());
        for n in ch {
            println!("  - ch.{n}");
        }
        return Ok(());
    }

    let rows = db::list_komik(&pool, limit).await?;
    println!("[DB] komik rows shown: {}", rows.len());
    for r in rows {
        println!(
            "- id={} slug={} judul={} latest={:?} chapter_count={:?}",
            r.id, r.slug, r.judul, r.latest_chapter_number, r.chapter_count
        );
        let ch = db::list_chapters(&pool, r.id, chapters).await.unwrap_or_default();
        if !ch.is_empty() {
            print!("  chapters: ");
            for (i, n) in ch.iter().enumerate() {
                if i > 0 {
                    print!(", ");
                }
                print!("{n}");
            }
            println!();
        }
    }

    Ok(())
}

async fn run_db_detail(slug: String, chapters: i64) -> Result<()> {
    let cfg = config::env_config();
    if cfg.database_url.is_empty() {
        anyhow::bail!("DATABASE_URL kosong. Set DATABASE_URL di .env lalu coba lagi.");
    }
    let pool = db::connect(&cfg.database_url).await?;

    let Some(detail) = db::get_komik_detail_by_slug(&pool, &slug, chapters).await? else {
        anyhow::bail!("slug tidak ditemukan di DB: {slug}");
    };

    let json = serde_json::to_string_pretty(&detail)?;
    println!("{json}");
    Ok(())
}

async fn db_connect_from_env() -> Result<sqlx::PgPool> {
    let cfg = config::env_config();
    if cfg.database_url.is_empty() {
        anyhow::bail!("DATABASE_URL kosong. Set DATABASE_URL di .env lalu coba lagi.");
    }
    let pool = db::connect(&cfg.database_url).await?;
    Ok(pool)
}

async fn run_db_schema(table: String) -> Result<()> {
    let pool = db_connect_from_env().await?;
    let table = table.to_lowercase();
    let allowed = ["komik", "chapters", "komik_genres", "scrape_log"];
    if !allowed.contains(&table.as_str()) {
        anyhow::bail!("table tidak valid: {table}. Pilih: komik|chapters|komik_genres|scrape_log");
    }

    println!("{}", "=".repeat(60));
    println!("  DB SCHEMA: {table}");
    println!("{}", "=".repeat(60));

    let rows = sqlx::query(
        r#"
        SELECT
            column_name,
            data_type,
            is_nullable,
            COALESCE(column_default, '') AS column_default
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = $1
        ORDER BY ordinal_position
        "#,
    )
    .bind(&table)
    .fetch_all(&pool)
    .await?;

    if rows.is_empty() {
        println!("(no columns found; table may not exist)");
        return Ok(());
    }

    for r in rows {
        let name: String = r.get("column_name");
        let data_type: String = r.get("data_type");
        let nullable: String = r.get("is_nullable");
        let default_v: String = r.get("column_default");
        if default_v.is_empty() {
            println!("- {name}: {data_type} nullable={nullable}");
        } else {
            println!("- {name}: {data_type} nullable={nullable} default={default_v}");
        }
    }

    Ok(())
}

async fn run_db_reset(restart_identity: bool) -> Result<()> {
    let pool = db_connect_from_env().await?;

    println!("{}", "=".repeat(60));
    println!("  DB RESET (TRUNCATE)");
    println!("{}", "=".repeat(60));

    // TRUNCATE is fast and keeps schema; CASCADE to clear dependent rows.
    // Order doesn't matter with CASCADE, but list all main tables explicitly.
    let stmt = if restart_identity {
        "TRUNCATE TABLE komik, chapters, komik_genres, scrape_log RESTART IDENTITY CASCADE"
    } else {
        "TRUNCATE TABLE komik, chapters, komik_genres, scrape_log CASCADE"
    };

    sqlx::query(stmt).execute(&pool).await?;
    println!("[DB] OK: {stmt}");
    Ok(())
}

async fn run_db_setup() -> Result<()> {
    let pool = db_connect_from_env().await?;

    println!("{}", "=".repeat(60));
    println!("  DB SETUP (SCHEMA)");
    println!("{}", "=".repeat(60));

    db::setup_schema(&pool).await?;
    db::ensure_schema(&pool).await?;
    println!("[DB] OK: schema ensured");
    Ok(())
}

async fn run_db_drop_all() -> Result<()> {
    let pool = db_connect_from_env().await?;

    println!("{}", "=".repeat(60));
    println!("  DB DROP ALL (DANGEROUS)");
    println!("{}", "=".repeat(60));

    // Drop dependent tables first doesn't matter with CASCADE, but keep explicit list.
    sqlx::query("DROP TABLE IF EXISTS komik_genres CASCADE").execute(&pool).await?;
    sqlx::query("DROP TABLE IF EXISTS chapters CASCADE").execute(&pool).await?;
    sqlx::query("DROP TABLE IF EXISTS scrape_log CASCADE").execute(&pool).await?;
    sqlx::query("DROP TABLE IF EXISTS komik CASCADE").execute(&pool).await?;

    println!("[DB] OK: dropped komik, chapters, komik_genres, scrape_log");
    Ok(())
}

// ============================================================
// LM DETECT (experimental)
// ============================================================

async fn run_lm_detect(limit: usize, field: String) -> Result<()> {
    use komikindo_scraper::lm_selector::{LmDetector, FieldType};

    // Parse which fields to test
    let fields: Vec<FieldType> = match field.as_str() {
        "title" => vec![FieldType::Title],
        "rating" => vec![FieldType::Rating],
        "all" => vec![FieldType::Title, FieldType::Rating],
        other => anyhow::bail!("Unknown field: '{}'. Use: title, rating, or all", other),
    };

    println!("{}", "=".repeat(70));
    println!("  LM FIELD DETECTION (EXPERIMENTAL)");
    println!("  Fields: {}", fields.iter().map(|f| format!("{:?}", f).to_lowercase()).collect::<Vec<_>>().join(", "));
    println!("{}", "=".repeat(70));

    // Load ONNX models
    println!("[LM] Loading ONNX models...");
    let mut detector = LmDetector::new(&fields)?;
    println!("[LM] Models loaded successfully");

    // Find latest JSONL
    let data_dir = std::path::Path::new("data");
    let jsonl_file = jsonl::find_latest_jsonl(data_dir);
    
    if jsonl_file.is_none() {
        anyhow::bail!("No JSONL files found in data/. Run full-fetch first.");
    }
    let jsonl_file = jsonl_file.unwrap();
    println!("[LM] Using JSONL: {}", jsonl_file.display());

    let fetcher = Fetcher::new(30, None, false, 10)?;
    
    let mut field_stats: HashMap<String, (usize, usize, usize)> = HashMap::new();
    for ft in &fields {
        field_stats.insert(format!("{:?}", ft).to_lowercase(), (0, 0, 0)); // (match, mismatch, miss)
    }
    let mut total = 0usize;
    let mut fetch_fail = 0usize;

    let file = std::fs::File::open(&jsonl_file)?;
    let reader = std::io::BufReader::new(file);

    for line in std::io::BufRead::lines(reader) {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if val.get("_status").is_some() { continue; }
        }

        if let Ok(detail) = serde_json::from_str::<KomikDetail>(trimmed) {
            if total >= limit { break; }
            total += 1;

            let slug = &detail.slug;
            let url = format!("{BASE_URL}/komik/{slug}/");
            
            match fetcher.fetch_page(&url).await {
                Ok(html) => {
                    for ft in &fields {
                        let field_name = format!("{:?}", ft).to_lowercase();
                        let stats = field_stats.get_mut(&field_name).unwrap();

                        match ft {
                            FieldType::Title => {
                                let lm_result = detector.detect_title(&html);
                                let selector_val = detail.judul.as_deref().unwrap_or("(none)");
                                match lm_result {
                                    Some(lm) => {
                                        let lm_clean = lm.trim().to_lowercase();
                                        let sel_clean = selector_val.trim().to_lowercase();
                                        if lm_clean == sel_clean {
                                            stats.0 += 1;
                                            println!("  [✓ title] {}: \"{}\"", slug, selector_val);
                                        } else {
                                            stats.1 += 1;
                                            println!("  [✗ title] {}: LM=\"{}\" vs SELECTOR=\"{}\"", slug, lm, selector_val);
                                        }
                                    }
                                    None => {
                                        stats.2 += 1;
                                        println!("  [? title] {}: no detection, SELECTOR=\"{}\"", slug, selector_val);
                                    }
                                }
                            }
                            FieldType::Rating => {
                                let lm_result = detector.detect_rating(&html);
                                let selector_val = detail.rating;
                                match lm_result {
                                    Some(lm) => {
                                        let match_ok = match selector_val {
                                            Some(sel) => (lm - sel).abs() < 0.01,
                                            None => false,
                                        };
                                        if match_ok {
                                            stats.0 += 1;
                                            println!("  [✓ rating] {}: {:.2} (selector={:.2})", slug, lm, selector_val.unwrap_or(0.0));
                                        } else {
                                            stats.1 += 1;
                                            println!("  [✗ rating] {}: LM={:.2} vs SELECTOR={}", slug, lm,
                                                selector_val.map(|v| format!("{:.2}", v)).unwrap_or("(none)".to_string()));
                                        }
                                    }
                                    None => {
                                        stats.2 += 1;
                                        println!("  [? rating] {}: no detection, SELECTOR={}", slug,
                                            selector_val.map(|v| format!("{:.2}", v)).unwrap_or("(none)".to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    fetch_fail += 1;
                    println!("  [!] {}: fetch failed: {}", slug, e);
                }
            }
        }
    }

    println!();
    println!("{}", "=".repeat(70));
    println!("  LM DETECT RESULTS");
    println!("  Total tested:    {total}");
    println!("  Fetch failures:  {fetch_fail}");
    for ft in &fields {
        let field_name = format!("{:?}", ft).to_lowercase();
        let (m, mm, ms) = field_stats[&field_name];
        let tested = m + mm + ms;
        if tested > 0 {
            println!("  --- {} ---", field_name);
            println!("  Match:           {m} ({:.0}%)", m as f64 / tested as f64 * 100.0);
            println!("  Mismatch:        {mm}");
            println!("  No detection:    {ms}");
        }
    }
    println!("{}", "=".repeat(70));

    Ok(())
}

async fn run_bench_parse(
    kind: String,
    iters: u32,
    slug: Option<String>,
    timeout: u64,
    proxy: Option<String>,
    verbose: bool,
    max_in_flight: usize,
) -> Result<()> {
    use std::hint::black_box;

    let fetcher = Fetcher::new(timeout, proxy.as_deref(), verbose, max_in_flight)?;
    let t_fetch = Instant::now();

    let (label, html, parse_fn): (&'static str, String, Box<dyn Fn(&str) + Send + Sync>) =
        match kind.as_str() {
            "list" => {
                let url = format!("{BASE_URL}/daftar-manga/?list");
                let html = fetcher.fetch_page(&url).await?;
                (
                    "parse_komik_list",
                    html,
                    Box::new(|h| {
                        let v = parsers::parse_komik_list(h);
                        black_box(v.len());
                    }),
                )
            }
            "homepage" => {
                let html = fetcher.fetch_page(BASE_URL).await?;
                (
                    "parse_homepage_updates",
                    html,
                    Box::new(|h| {
                        let v = parsers::parse_homepage_updates(h);
                        black_box(v.len());
                    }),
                )
            }
            "terbaru" => {
                let url = format!("{BASE_URL}/komik-terbaru/");
                let html = fetcher.fetch_page(&url).await?;
                (
                    "parse_komik_terbaru",
                    html,
                    Box::new(|h| {
                        let v = parsers::parse_komik_terbaru(h);
                        black_box(v.len());
                    }),
                )
            }
            "detail" => {
                let slug = slug.unwrap_or_else(|| "one-piece".to_string());
                let url = format!("{BASE_URL}/komik/{slug}/");
                let html = fetcher.fetch_page(&url).await?;
                (
                    "parse_komik_detail",
                    html,
                    Box::new(move |h| {
                        let v = parsers::parse_komik_detail(&slug, h);
                        black_box(v.as_ref().map(|d| d.chapters.len()).unwrap_or(0));
                    }),
                )
            }
            _ => anyhow::bail!("Unknown kind: {kind}"),
        };

    let fetch_secs = t_fetch.elapsed().as_secs_f64();
    println!("[BENCH] fetched HTML in {:.3}s ({} bytes)", fetch_secs, html.len());
    println!("[BENCH] running {label} iters={iters} ...");

    let t0 = Instant::now();
    for _ in 0..iters {
        parse_fn(black_box(&html));
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let per_iter_ms = elapsed / iters as f64 * 1000.0;
    let iters_per_sec = iters as f64 / elapsed.max(1e-9);
    println!(
        "[BENCH] {}: {:.3}s total | {:.3} ms/iter | {:.1} iters/s",
        label, elapsed, per_iter_ms, iters_per_sec
    );

    Ok(())
}

// ============================================================
// DEBUG (diagnostics)
// ============================================================

struct DebugOpts {
    all: bool,
    db: bool,
    network: bool,
    show_env: bool,
    info: bool,
    proxy: Option<String>,
    timeout: u64,
    verbose: bool,
    max_in_flight: usize,
}

async fn run_debug(opts: DebugOpts) -> Result<()> {
    let run_all = opts.all || (!opts.db && !opts.network && !opts.show_env && !opts.info);

    // --- Binary Info ---
    if run_all || opts.info {
        println!("{}", "=".repeat(50));
        println!("  BINARY / PLATFORM INFO");
        println!("{}", "=".repeat(50));

        // Binary path
        if let Ok(exe) = std::env::current_exe() {
            println!("  Binary:     {}", exe.display());
            if let Ok(meta) = std::fs::metadata(&exe) {
                let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
                println!("  Size:       {:.1} MB", size_mb);
            }
        }

        // Platform info
        println!("  OS:         {}", std::env::consts::OS);
        println!("  Arch:       {}", std::env::consts::ARCH);
        println!("  Family:     {}", std::env::consts::FAMILY);

        // Target triple (compile-time)
        #[cfg(target_os = "linux")]
        println!("  Target:     {}-{}", std::env::consts::ARCH, std::env::consts::OS);
        #[cfg(target_os = "android")]
        println!("  Target:     {}-android (Termux-compatible)", std::env::consts::ARCH);

        // Check for Termux
        if let Ok(prefix) = std::env::var("PREFIX") {
            println!("  Termux:     YES (PREFIX={})", prefix);
        } else {
            println!("  Termux:     NO");
        }

        // CWD
        if let Ok(cwd) = std::env::current_dir() {
            println!("  CWD:        {}", cwd.display());
        }

        // CA certificate bundle (critical for SSL)
        match fetcher::find_ca_bundle() {
            Some(ref p) => println!("  CA bundle:  {}", p),
            None => {
                println!("  CA bundle:  NOT FOUND!");
                if let Ok(prefix) = std::env::var("PREFIX") {
                    println!("    → Try: pkg install ca-certificates");
                    println!("    → Or:  export SSL_CERT_FILE={}/etc/tls/cert.pem", prefix);
                } else {
                    println!("    → Try: sudo apt install ca-certificates");
                    println!("    → Or:  export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt");
                }
            }
        }

        println!();
    }

    // --- Environment Info ---
    if run_all || opts.show_env {
        println!("{}", "=".repeat(50));
        println!("  ENVIRONMENT / CONFIG");
        println!("{}", "=".repeat(50));

        let cfg = config::env_config();
        let load_info = config::env_load_info();

        // .env file search results
        println!("\n  .env file search ({} paths checked):", load_info.searched_paths.len());
        for (i, p) in load_info.searched_paths.iter().enumerate() {
            let marker = if load_info.loaded_path.as_deref() == Some(p.as_str()) {
                " <<< FOUND"
            } else {
                ""
            };
            println!("    {}. {}{}", i + 1, p, marker);
        }

        match &load_info.loaded_path {
            Some(p) => println!("\n  .env loaded: YES -> {}", p),
            None => println!("\n  .env loaded: NO (use --env /path/to/.env)"),
        }

        // Show config values (masked)
        println!("\n  Config:");
        println!("    DATABASE_URL:      {}", config::mask_database_url(&cfg.database_url));
        println!("    PROXY_URL:         {}", if cfg.proxy_url.is_empty() { "(not set)".to_string() } else { cfg.proxy_url.clone() });
        println!("    PROXY_ENABLED:     {}", cfg.proxy_enabled);
        println!("    SCRAPER_RETRIES:   {}", cfg.scraper_retries);
        println!("    SCRAPER_TIMEOUT:   {}s", cfg.scraper_timeout);
        println!("    DB mode:           {}", if !cfg.database_url.is_empty() { "ENABLED" } else { "DISABLED (no DATABASE_URL)" });

        // Check if .env exists next to binary
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let env_at_binary = dir.join(".env");
                println!("\n  .env next to binary: {}", if env_at_binary.exists() { "YES" } else { "NO" });
            }
        }

        // Check PROXY_URL from .env vs CLI
        if let Some(ref cli_proxy) = opts.proxy {
            println!("\n  CLI proxy override: {}", cli_proxy);
        }

        println!();
    }

    // --- Network Test ---
    if run_all || opts.network {
        println!("{}", "=".repeat(50));
        println!("  NETWORK TEST");
        println!("{}", "=".repeat(50));

        let cfg = config::env_config();
        let effective_proxy = opts.proxy.clone()
            .or_else(|| if cfg.proxy_enabled && !cfg.proxy_url.is_empty() { Some(cfg.proxy_url.clone()) } else { None });

        // Test 1: Direct (no proxy)
        if effective_proxy.is_none() || run_all {
            println!("\n  --- Test 1: Direct connection ---");
            let t0 = Instant::now();
            let fetcher_direct = Fetcher::new(opts.timeout, None, opts.verbose, opts.max_in_flight)?;
            match fetcher_direct.fetch_page(BASE_URL).await {
                Ok(html) => {
                    let elapsed = t0.elapsed();
                    println!("  [OK] komikindo.ch: {:.1} KB in {:.3}s",
                        html.len() as f64 / 1024.0, elapsed.as_secs_f64());
                    if html.contains("Just a moment...") {
                        println!("  [WARN] Cloudflare challenge detected!");
                    }
                }
                Err(e) => {
                    let elapsed = t0.elapsed();
                    println!("  [FAIL] komikindo.ch: {} ({:.3}s)", e, elapsed.as_secs_f64());
                    println!("  [HINT] Direct connection blocked. Use --proxy");
                }
            }
        }

        // Test 2: With proxy
        let proxy_to_test = effective_proxy.as_deref();
        if let Some(proxy) = proxy_to_test {
            println!("\n  --- Test 2: Proxy connection ({}) ---", proxy);
            let t0 = Instant::now();
            let fetcher_proxy = Fetcher::new(opts.timeout, Some(proxy), opts.verbose, opts.max_in_flight)?;
            match fetcher_proxy.fetch_page(BASE_URL).await {
                Ok(html) => {
                    let elapsed = t0.elapsed();
                    println!("  [OK] komikindo.ch via proxy: {:.1} KB in {:.3}s",
                        html.len() as f64 / 1024.0, elapsed.as_secs_f64());
                    if html.contains("Just a moment...") {
                        println!("  [WARN] Cloudflare challenge detected even with proxy!");
                    }
                }
                Err(e) => {
                    let elapsed = t0.elapsed();
                    println!("  [FAIL] komikindo.ch via proxy: {} ({:.3}s)", e, elapsed.as_secs_f64());
                    println!("  [HINT] Check proxy is running. Try socks5h:// for remote DNS.");
                }
            }
        } else if !run_all {
            println!("\n  [SKIP] No proxy configured. Use --proxy or set PROXY_URL in .env");
        }

        // Test 3: /komik-terbaru/ (quick content check)
        println!("\n  --- Test 3: Content check (/komik-terbaru/) ---");
        let fetcher = Fetcher::new(opts.timeout, proxy_to_test, opts.verbose, opts.max_in_flight)?;
        let t0 = Instant::now();
        match fetcher.fetch_page(&format!("{BASE_URL}/komik-terbaru/")).await {
            Ok(html) => {
                let elapsed = t0.elapsed();
                println!("  [OK] /komik-terbaru/: {:.1} KB in {:.3}s",
                    html.len() as f64 / 1024.0, elapsed.as_secs_f64());
                // Quick check: count entries
                let entry_count = html.matches("class=\"entry-item\"").count()
                    + html.matches("<article").count();
                if entry_count > 0 {
                    println!("  [OK] Found ~{} entries", entry_count);
                }
            }
            Err(e) => {
                println!("  [FAIL] /komik-terbaru/: {}", e);
            }
        }

        println!();
    }

    // --- DB Test ---
    if run_all || opts.db {
        println!("{}", "=".repeat(50));
        println!("  DATABASE TEST");
        println!("{}", "=".repeat(50));

        let cfg = config::env_config();
        if cfg.database_url.is_empty() {
            println!("\n  [SKIP] DATABASE_URL not set");
            println!("  [HINT] Set DATABASE_URL in .env or use --env /path/to/.env");
        } else {
            println!("\n  URL: {}", config::mask_database_url(&cfg.database_url));
            println!("  Connecting...");
            let t0 = Instant::now();
            match db::connect(&cfg.database_url).await {
                Ok(pool) => {
                    let elapsed = t0.elapsed();
                    println!("  [OK] Connected in {:.3}s", elapsed.as_secs_f64());

                    // Test query: count komik
                    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM komik")
                        .fetch_one(&pool).await
                    {
                        Ok(count) => println!("  [OK] Total komik in DB: {}", count),
                        Err(e) => println!("  [WARN] COUNT query failed: {}", e),
                    }

                    // Test query: count chapters
                    match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chapters")
                        .fetch_one(&pool).await
                    {
                        Ok(count) => println!("  [OK] Total chapters in DB: {}", count),
                        Err(e) => println!("  [WARN] COUNT chapters failed: {}", e),
                    }

                    // Test query: latest scrape_log
                    match sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
                        "SELECT operation, status, created_at FROM scrape_log ORDER BY id DESC LIMIT 1"
                    )
                    .fetch_optional(&pool).await
                    {
                        Ok(Some((op, status, ts))) => {
                            println!("  [OK] Last scrape: {} ({}) at {}", op, status, ts);
                        }
                        Ok(None) => println!("  [OK] No scrape_log entries yet"),
                        Err(e) => println!("  [WARN] scrape_log query failed: {}", e),
                    }
                }
                Err(e) => {
                    let elapsed = t0.elapsed();
                    println!("  [FAIL] Connection failed after {:.3}s", elapsed.as_secs_f64());
                    println!("  Error: {}", e);
                    println!("\n  Troubleshooting:");
                    println!("    - Check DATABASE_URL format: postgresql://user:pass@host:5432/db");
                    println!("    - For Supabase: use port 5432 (not 6543 PgBouncer)");
                    println!("    - Check network connectivity to the DB host");
                    println!("    - Verify credentials are correct");
                }
            }
        }

        println!();
    }

    println!("{}", "=".repeat(50));
    println!("  DEBUG COMPLETE");
    println!("{}", "=".repeat(50));

    Ok(())
}

// ============================================================
// CHECK (connectivity test)
// ============================================================

async fn run_check(
    proxy: &Option<String>,
    timeout: u64,
    verbose: bool,
    max_in_flight: usize,
) -> Result<()> {
    println!("{}", "=".repeat(50));
    println!("  KOMIKINDO CONNECTIVITY CHECK");
    if let Some(ref p) = proxy {
        println!("  Proxy: {p}");
    } else {
        println!("  Proxy: (none — direct connection)");
    }
    if verbose {
        println!("  Verbose: ON (curl protocol details below)");
    }
    println!("{}", "=".repeat(50));

    let fetcher = Fetcher::new(timeout, proxy.as_deref(), verbose, max_in_flight)?;

    // Test 1: Homepage
    println!("\n--- Test 1: Fetch homepage ---");
    let t0 = Instant::now();
    match fetcher.fetch_page(BASE_URL).await {
        Ok(html) => {
            let elapsed = t0.elapsed();
            let size_kb = html.len() as f64 / 1024.0;
            println!("[OK] Homepage fetched: {:.1} KB in {:.3}s", size_kb, elapsed.as_secs_f64());
            if html.contains("Just a moment...") || html.contains("cf-challenge") {
                println!("[WARN] Cloudflare challenge page detected — scraping will fail!");
            } else if html.len() < 500 {
                println!("[WARN] Very small response ({} bytes) — might be blocked or error page", html.len());
                println!("[WARN] Response preview: {}", &html[..html.len().min(200)]);
            } else {
                println!("[OK] Response looks normal ({} bytes)", html.len());
            }
        }
        Err(e) => {
            let elapsed = t0.elapsed();
            println!("[FAIL] Homepage fetch failed after {:.3}s", elapsed.as_secs_f64());
            println!("[FAIL] Error: {e}");
            println!();
            println!("Troubleshooting:");
            if proxy.is_none() {
                println!("  - Try with --proxy socks5://127.0.0.1:PORT");
            } else {
                println!("  - Check proxy is running and accessible");
                println!("  - Try socks5h:// instead of socks5:// (remote DNS resolution)");
            }
            println!("  - Use --verbose for full curl protocol details");
            println!("  - komikindo.ch may block datacenter/VPN IPs (Cloudflare)");
        }
    }

    // Test 2: /komik-terbaru/
    println!("\n--- Test 2: Fetch /komik-terbaru/ ---");
    let t0 = Instant::now();
    match fetcher.fetch_page(&format!("{BASE_URL}/komik-terbaru/")).await {
        Ok(html) => {
            let elapsed = t0.elapsed();
            println!("[OK] /komik-terbaru/ fetched: {:.1} KB in {:.3}s",
                html.len() as f64 / 1024.0, elapsed.as_secs_f64());
        }
        Err(e) => {
            println!("[FAIL] /komik-terbaru/ fetch failed: {e}");
        }
    }

    // Stats
    let stats = fetcher.stats();
    println!("\n--- Fetcher Stats ---");
    println!("  Requests:  {}", stats.requests);
    println!("  Success:   {}", stats.success);
    println!("  Failed:    {}", stats.failed);
    println!("  Retries:   {}", stats.retries);
    println!("  Downloaded: {:.1} KB", stats.bytes_downloaded as f64 / 1024.0);

    Ok(())
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
// FULL FETCH (dengan optional chapter image scraping)
// ============================================================

struct FullFetchOpts {
    limit: usize,
    start_from: Option<String>,
    resume: bool,
    timeout: u64,
    proxy: Option<String>,
    verbose: bool,
    max_in_flight: usize,
    auto_upload: bool,
    with_images: bool,
}

async fn run_full_fetch(opts: FullFetchOpts) -> Result<()> {
    let start_time = Instant::now();
    let dt_start = Local::now();

    let data_dir = PathBuf::from("data");
    std::fs::create_dir_all(&data_dir)?;

    // JSONL path
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

    let proxy_url = opts.proxy.as_deref();
    let cfg = config::env_config();
    let proxy_info = if let Some(p) = proxy_url {
        format!("\n  Proxy: {p}")
    } else if cfg.proxy_enabled && !cfg.proxy_url.is_empty() {
        format!("\n  Proxy: {}", cfg.proxy_url)
    } else {
        String::new()
    };

    let mode_label = if opts.with_images { "DENGAN image (two-phase)" } else { "tanpa image" };
    println!("{}", "=".repeat(70));
    println!("  KOMIKINDO FULL FETCH → JSONL (no DB)");
    println!("  Started at {}", dt_start.format("%Y-%m-%d %H:%M:%S"));
    println!("  Timeout: {}s", opts.timeout);
    println!("  Mode: {mode_label}");
    println!("  Storage: JSONL file");
    println!("  In-flight requests: {}", opts.max_in_flight);
    println!("  NOTE: Gunakan 'upload-db' untuk upload ke database setelah selesai");
    println!("{proxy_info}");
    println!("{}", "=".repeat(70));

    let fetcher = Arc::new(Fetcher::new(
        opts.timeout,
        proxy_url,
        opts.verbose,
        opts.max_in_flight,
    )?);

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

    // === FETCH ALL → JSONL (pure speed, no DB write) ===
    println!("\n--- FETCH ALL → JSONL ---");
    println!("[INFO] Fetching {} komik details", total);

    let success = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let total_chapters = Arc::new(AtomicUsize::new(0));
    let jsonl_path = Arc::new(jsonl_path);
    let writer_fetcher = Arc::clone(&fetcher);

    // Sliding window with JoinSet — eliminates chunk barriers for continuous pipelining.
    // The Fetcher's internal semaphore already limits actual HTTP concurrency,
    // so we maintain a window of max_in_flight spawned tasks for optimal throughput.
    // As each task completes, we immediately spawn the next one — no idle gaps.
    let window_size = opts.max_in_flight.min(total);
    println!("[INFO] Sliding window: {} in-flight tasks (no chunk barriers)", window_size);

    // Use buffered JSONL writer (keeps file open, 1MB buffer)
    let buffered_writer = Arc::new(jsonl::BufferedJsonlWriter::new(&jsonl_path)?);
    let bw = Arc::clone(&buffered_writer);

    let write_success = Arc::clone(&success);
    let write_failed = Arc::clone(&failed);
    let write_ch = Arc::clone(&total_chapters);

    // ===================================================================
    // PHASE 1: Fetch semua komik detail → JSONL (TANPA image, super cepat)
    //
    // Two-phase pipeline untuk --with-images:
    //   Phase 1: Detail saja → ~4500 komik/min (1 request per komik)
    //   Phase 2: Chapter images paralel → menggunakan sliding window
    //            penuh untuk chapter URLs (bukan per-komik sequential)
    //
    // Sebelumnya: sequential chapter fetch per komik → 79 komik/min
    //   (setiap komik block sliding window slot sampai semua chapter
    //    selesai di-fetch = 50+ request sequential per komik)
    // ===================================================================
    let fetch_task = tokio::spawn(async move {
        let mut join_set = tokio::task::JoinSet::new();
        let mut slug_iter = komik_list.into_iter();

        // Fill initial window
        for slug in slug_iter.by_ref().take(window_size) {
            let fetcher = Arc::clone(&writer_fetcher);
            join_set.spawn(async move {
                let slug_clone = slug.clone();
                let result = scrape_komik_detail(slug, &fetcher).await;
                (slug_clone, result)
            });
        }

        // Process results and spawn new tasks as slots free up
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((slug, detail_result)) => {
                    match detail_result {
                        Ok(detail) => {
                            let ch_count = detail.chapters.len();
                            write_ch.fetch_add(ch_count, Ordering::Relaxed);

                            // Phase 1: tulis detail tanpa image data (cepat!)
                            // Phase 2 akan enrich dengan image data nanti
                            if let Err(e) = bw.append(&detail) {
                                eprintln!("  [WARN] JSONL write error: {e}");
                            }

                            write_success.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            write_failed.fetch_add(1, Ordering::Relaxed);
                            let failed_json = serde_json::json!({
                                "slug": slug,
                                "_status": "detail_failed",
                                "_error": e.to_string(),
                            });
                            let _ = bw.append_raw(&failed_json.to_string());
                        }
                    }
                }
                Err(_) => {
                    write_failed.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Spawn next task if there are more slugs
            if let Some(slug) = slug_iter.next() {
                let fetcher = Arc::clone(&writer_fetcher);
                join_set.spawn(async move {
                    let slug_clone = slug.clone();
                    let result = scrape_komik_detail(slug, &fetcher).await;
                    (slug_clone, result)
                });
            }
        }
    });

    // Progress reporter (Phase 1 only — no image info during detail fetch)
    let progress_total = total;
    let progress_start = start_time;
    let progress_fetcher = Arc::clone(&fetcher);
    let progress_success = Arc::clone(&success);
    let progress_failed = Arc::clone(&failed);
    let progress_ch = Arc::clone(&total_chapters);

    let progress_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let s = progress_success.load(Ordering::Relaxed);
            let f = progress_failed.load(Ordering::Relaxed);
            let c = s + f;
            if c == 0 { continue; }
            let elapsed = progress_start.elapsed().as_secs_f64();
            let rate = c as f64 / elapsed * 60.0;
            let eta = if rate > 0.0 {
                (progress_total - c) as f64 / rate
            } else {
                0.0
            };
            let stats = progress_fetcher.stats();
            eprint!(
                "  [PHASE 1 {}/{} {:.1}%] {:.0} komik/min | ETA: {:.0}min | \
                 OK: {} FAIL: {} | Ch: {} | DL: {:.1}MB | req: {}\r",
                c, progress_total, c as f64 / progress_total as f64 * 100.0,
                rate, eta, s, f,
                progress_ch.load(Ordering::Relaxed),
                stats.mb_downloaded(), stats.requests,
            );
        }
    });

    // Periodic JSONL flush (every 30 seconds to prevent data loss on crash)
    let flush_writer = Arc::clone(&buffered_writer);
    let flush_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Err(e) = flush_writer.flush() {
                eprintln!("  [WARN] Periodic JSONL flush error: {e}");
            }
        }
    });

    // Wait for fetch to complete
    let _ = fetch_task.await;
    progress_handle.abort();
    flush_handle.abort();

    // Final flush buffered JSONL writer to ensure all data is on disk
    buffered_writer.flush()?;

    let fetch_elapsed = start_time.elapsed().as_secs_f64();
    let mut stats = fetcher.stats();
    let s = success.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let ch = total_chapters.load(Ordering::Relaxed);
    let mut img = 0usize;

    println!();
    println!("  [PHASE 1 DONE] {s} komik, {f} failed, {ch} chapters in {:.1}s ({:.1} min)",
        fetch_elapsed, fetch_elapsed / 60.0);
    println!("  [PHASE 1 RATE] {:.1} komik/min", s as f64 / fetch_elapsed * 60.0);

    // ===================================================================
    // PHASE 2: Fetch chapter images (jika --with-images)
    //
    // Baca JSONL, kumpulkan semua chapter URL, lalu fetch images secara
    // paralel menggunakan sliding window (sama seperti Phase 1).
    //
    // Keuntungan dibanding sequential per-komik:
    //   - Sliding window 200 request paralel di semua chapter dari semua komik
    //   - Tidak ada blocking per-komik (sebelumnya 50+ request sequential per slot)
    //   - Throughput: ~200 chapter images/detik vs ~5/detik (sequential)
    // ===================================================================
    // Track Phase 2 stats for summary (populated only if --with-images)
    let mut phase2_elapsed = 0.0_f64;
    let mut phase2_ch_ok = 0usize;
    let mut phase2_ch_fail = 0usize;

    if opts.with_images && s > 0 {
        println!("\n{}", "=".repeat(70));
        println!("  PHASE 2: Fetching chapter images (paralel sliding window)");
        println!("{}", "=".repeat(70));

        let phase2_start = Instant::now();

        // Baca JSONL → Vec<(slug, Vec<ChapterInfo>)>
        let jsonl_file = jsonl_path.as_ref();
        let komik_chapters = {
            let file = std::fs::File::open(jsonl_file)?;
            let reader = std::io::BufReader::new(file);
            let mut chapters_map: Vec<(String, Vec<parsers::ChapterInfo>)> = Vec::new();
            for line in std::io::BufRead::lines(reader) {
                let Ok(line) = line else { continue };
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }
                if let Ok(detail) = serde_json::from_str::<KomikDetail>(trimmed) {
                    if !detail.chapters.is_empty() {
                        chapters_map.push((detail.slug.clone(), detail.chapters));
                    }
                }
            }
            chapters_map
        };

        // Kumpulkan semua chapter URL dengan index mapping
        // (komik_idx, chapter_idx, chapter_url)
        let mut all_chapter_urls: Vec<(usize, usize, String)> = Vec::new();
        for (kidx, (_, chapters)) in komik_chapters.iter().enumerate() {
            for (cidx, ch) in chapters.iter().enumerate() {
                all_chapter_urls.push((kidx, cidx, ch.url.clone()));
            }
        }

        let total_chapters_to_fetch = all_chapter_urls.len();
        let total_komik_with_ch = komik_chapters.len();
        println!("[PHASE 2] {total_chapters_to_fetch} chapters from {total_komik_with_ch} komik to fetch");

        // Result storage: Vec<Option<ChapterImageData>>
        let img_results: Arc<std::sync::Mutex<Vec<Option<parsers::ChapterImageData>>>> =
            Arc::new(std::sync::Mutex::new(vec![None; total_chapters_to_fetch]));

        let img_success = Arc::new(AtomicUsize::new(0));
        let img_failed = Arc::new(AtomicUsize::new(0));
        let img_total_images = Arc::new(AtomicUsize::new(0));

        // Sliding window untuk chapter image fetching
        // Gunakan setengah dari max_in_flight untuk mengurangi rate limiting
        // (chapter pages lebih banyak → lebih mudah trigger 429)
        let img_window_size = (opts.max_in_flight / 2).max(50).min(total_chapters_to_fetch);
        println!("[PHASE 2] Sliding window: {} in-flight chapter requests", img_window_size);

        let phase2_fetcher = Arc::clone(&fetcher);
        let phase2_urls = Arc::new(all_chapter_urls);
        let phase2_results = Arc::clone(&img_results);
        let phase2_img_success = Arc::clone(&img_success);
        let phase2_img_failed = Arc::clone(&img_failed);
        let phase2_img_total = Arc::clone(&img_total_images);

        let phase2_task = tokio::spawn(async move {
            let mut join_set = tokio::task::JoinSet::new();
            let mut url_iter = (0..total_chapters_to_fetch).into_iter();

            // Fill initial window
            for idx in url_iter.by_ref().take(img_window_size) {
                let fetcher = Arc::clone(&phase2_fetcher);
                let (_, _, url) = &phase2_urls[idx];
                let url = url.clone();
                join_set.spawn(async move {
                    let result = scrape_chapter_images(&url, &fetcher).await;
                    (idx, result)
                });
            }

            // Process results and spawn new tasks as slots free up
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok((idx, img_result)) => {
                        match img_result {
                            Ok(data) => {
                                let count = data.total_images;
                                phase2_img_total.fetch_add(count, Ordering::Relaxed);
                                phase2_img_success.fetch_add(1, Ordering::Relaxed);
                                // Store result
                                if let Ok(mut results) = phase2_results.lock() {
                                    results[idx] = Some(data);
                                }
                            }
                            Err(_) => {
                                phase2_img_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {
                        phase2_img_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // Spawn next task if there are more URLs
                if let Some(idx) = url_iter.next() {
                    let fetcher = Arc::clone(&phase2_fetcher);
                    let (_, _, url) = &phase2_urls[idx];
                    let url = url.clone();
                    join_set.spawn(async move {
                        let result = scrape_chapter_images(&url, &fetcher).await;
                        (idx, result)
                    });
                }
            }
        });

        // Progress reporter for Phase 2
        let p2_total = total_chapters_to_fetch;
        let p2_start = phase2_start;
        let p2_fetcher = Arc::clone(&fetcher);
        let p2_success = Arc::clone(&img_success);
        let p2_failed = Arc::clone(&img_failed);
        let p2_img_total = Arc::clone(&img_total_images);

        let p2_progress = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let s2 = p2_success.load(Ordering::Relaxed);
                let f2 = p2_failed.load(Ordering::Relaxed);
                let c2 = s2 + f2;
                if c2 == 0 { continue; }
                let elapsed = p2_start.elapsed().as_secs_f64();
                let rate = c2 as f64 / elapsed;
                let eta = if rate > 0.0 {
                    (p2_total - c2) as f64 / rate / 60.0
                } else {
                    0.0
                };
                let stats2 = p2_fetcher.stats();
                eprint!(
                    "  [PHASE 2 {}/{} {:.1}%] {:.0} ch/min | ETA: {:.0}min | \
                     OK: {} FAIL: {} | Img: {} | DL: {:.1}MB | req: {}\r",
                    c2, p2_total, c2 as f64 / p2_total as f64 * 100.0,
                    rate * 60.0, eta, s2, f2,
                    p2_img_total.load(Ordering::Relaxed),
                    stats2.mb_downloaded(), stats2.requests,
                );
            }
        });

        // Wait for Phase 2 to complete
        let _ = phase2_task.await;
        p2_progress.abort();

        phase2_elapsed = phase2_start.elapsed().as_secs_f64();
        phase2_ch_ok = img_success.load(Ordering::Relaxed);
        phase2_ch_fail = img_failed.load(Ordering::Relaxed);
        img = img_total_images.load(Ordering::Relaxed);
        stats = fetcher.stats();

        println!();
        println!("  [PHASE 2 DONE] {phase2_ch_ok} chapters OK, {phase2_ch_fail} failed in {:.1}s ({:.1} min)",
            phase2_elapsed, phase2_elapsed / 60.0);
        println!("  [PHASE 2 RATE] {:.0} chapters/min, {img} total images",
            phase2_ch_ok as f64 / phase2_elapsed * 60.0);

        // === Rewrite JSONL with image data ===
        println!("\n  [MERGE] Rewriting JSONL with image data...");
        let merge_start = Instant::now();

        let results_guard = img_results.lock().unwrap();

        // Baca ulang JSONL dan enrich dengan image data
        let jsonl_file = jsonl_path.as_ref();
        let file = std::fs::File::open(jsonl_file)?;
        let reader = std::io::BufReader::new(file);

        let mut updated_details: Vec<KomikDetail> = Vec::with_capacity(s);
        let mut chapter_result_idx = 0usize;

        for line in std::io::BufRead::lines(reader) {
            let Ok(line) = line else { continue };
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }

            // Skip error entries
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if val.get("_status").is_some() {
                    continue;
                }
            }

            if let Ok(mut detail) = serde_json::from_str::<KomikDetail>(trimmed) {
                // Temukan chapter results untuk komik ini
                // Karena kita iterasi dalam urutan yang sama, chapter_result_idx
                // akan sesuai dengan all_chapter_urls yang kita buat di atas
                for chapter in &mut detail.chapters {
                    if chapter_result_idx < results_guard.len() {
                        if let Some(ref data) = results_guard[chapter_result_idx] {
                            chapter.set_image_data(data.clone());
                        }
                        chapter_result_idx += 1;
                    }
                }
                updated_details.push(detail);
            }
        }

        drop(results_guard);

        // Tulis ulang JSONL ke temporary file, lalu replace file asli
        // (BufferedJsonlWriter buka dalam append mode, jadi harus pakai file baru)
        let tmp_jsonl_path = jsonl_file.with_extension("jsonl.tmp");
        let tmp_file = std::fs::File::create(&tmp_jsonl_path)?;
        let mut tmp_writer = std::io::BufWriter::with_capacity(1024 * 1024, tmp_file);

        for detail in &updated_details {
            let json = serde_json::to_string(detail).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            writeln!(tmp_writer, "{}", json)?;
        }
        tmp_writer.flush()?;

        // Atomic rename: replace original file with enriched version
        std::fs::rename(&tmp_jsonl_path, jsonl_file)?;

        let merge_elapsed = merge_start.elapsed().as_secs_f64();
        let jsonl_size_mb = std::fs::metadata(jsonl_file)
            .map(|m| m.len() as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0);
        println!("  [MERGE DONE] {} komik enriched in {:.1}s → {:.1} MB",
            updated_details.len(), merge_elapsed, jsonl_size_mb);
    }

    let jsonl_size_mb = std::fs::metadata(jsonl_path.as_ref())
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    // === Summary ===
    let total_elapsed = start_time.elapsed().as_secs_f64();

    println!();
    println!("{}", "=".repeat(70));
    println!("  FULL FETCH COMPLETE");
    println!("  Total komik:    {total}");
    println!("  Success:        {s}");
    println!("  Failed:         {f}");
    println!("  Chapters:       {ch}");
    if opts.with_images {
        println!("  Images:         {img}");
    }
    println!("  Phase 1 time:   {:.1}s ({:.1}min) | {:.1} komik/min",
        fetch_elapsed, fetch_elapsed / 60.0,
        s as f64 / fetch_elapsed * 60.0);
    if opts.with_images && phase2_elapsed > 0.0 {
        println!("  Phase 2 time:   {:.1}s ({:.1}min) | {:.0} ch/min | {phase2_ch_ok} OK {phase2_ch_fail} FAIL",
            phase2_elapsed, phase2_elapsed / 60.0,
            phase2_ch_ok as f64 / phase2_elapsed * 60.0);
    }
    println!("  Total time:     {:.1}s ({:.1}min)",
        total_elapsed, total_elapsed / 60.0);
    println!("  Requests:       {}", stats.requests);
    println!("  Retries:        {}", stats.retries);
    println!("  Downloaded:     {:.1} MB", stats.mb_downloaded());
    println!("  JSONL:          {} ({:.1} MB)",
        jsonl_path.display(), jsonl_size_mb);
    if s > 0 && !opts.auto_upload {
        println!();
        println!("  >>> Untuk upload ke DB: komikindo-scraper upload-db <<<");
    }
    println!("{}", "=".repeat(70));

    // === Auto Upload ===
    if opts.auto_upload && s > 0 {
        println!("\n[AUTO-UPLOAD] Starting automatic DB upload...");
        let jsonl_path_str = jsonl_path.to_string_lossy().to_string();
        match run_upload_db(Some(jsonl_path_str), 500).await {
            Ok(()) => println!("[AUTO-UPLOAD] Complete!"),
            Err(e) => eprintln!("[AUTO-UPLOAD] Failed: {e}"),
        }
    }

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
    verbose: bool,
    max_in_flight: usize,
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

    let fetcher = Arc::new(Fetcher::new(
        opts.timeout,
        opts.proxy.as_deref(),
        opts.verbose,
        opts.max_in_flight,
    )?);

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
                let slug_clone = slug.clone();
                let result = scrape_komik_detail(slug, &fetcher).await;
                (slug_clone, result)
            }));
        }

        let mut new_success = 0usize;
        let mut new_failed = 0usize;
        let mut new_details: Vec<KomikDetail> = Vec::new();

        for handle in detail_handles {
            match handle.await {
                Ok((slug, Ok(detail))) => {
                    println!("[NEW] + {} — {}", slug, detail.judul.as_deref().unwrap_or("?"));
                    new_success += 1;
                    new_details.push(detail);
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

        // Batch write all new komik to DB — single UNNEST query instead of N sequential writes.
        // With 20 new komiks × ~1s per write_komik (4-5 DB round-trips each),
        // batch_write_komik reduces this to ~2-3 total queries.
        if let Some((pool, _)) = &db_pool {
            if !opts.dry_run && !new_details.is_empty() {
                match db::batch_write_komik(pool, &new_details).await {
                    Ok(result) => println!("[DB] Batch wrote {} new, {} updated",
                        result.new_count, result.updated_count),
                    Err(e) => eprintln!("[DB ERR] Batch write failed: {e}"),
                }
            }
        }

        // Update JSONL after batch
        for detail in &new_details {
            jsonl_db.insert(detail.slug.clone(), detail.clone());
        }

        println!("[NEW] {} success, {} failed", new_success, new_failed);
    }

    // === Step 5: Update latest_chapter_number in DB for chapter-only updates ===
    // Batch UNNEST UPDATE instead of N individual queries — saves ~200ms × N.
    if !opts.dry_run {
        if let Some((pool, _)) = &db_pool {
            if !updated_chapters.is_empty() {
                println!("\n--- Step 5: Updating latest_chapter_number in DB ---");
                let batch_updates: Vec<(i32, f64)> = updated_chapters.iter()
                    .filter_map(|(slug, _, new_ch, _)| {
                        db_chapter_map.get(slug).map(|&(id, _)| (id, *new_ch))
                    })
                    .collect();
                match db::batch_update_latest_chapters(pool, &batch_updates).await {
                    Ok(n) => println!("[DB] Updated latest_chapter_number for {n} komik (batch)"),
                    Err(e) => eprintln!("[DB WARN] Batch update failed: {e}"),
                }
            }
        }
    }

    // === Step 6: Save ===
    if opts.dry_run {
        println!("\n--- DRY RUN: skipping save ---");
    } else {
        println!("\n--- Step 6: Saving ---");

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
// UPLOAD DB (manual batch from JSONL → DB)
// ============================================================

async fn run_upload_db(file: Option<String>, batch_size: usize) -> Result<()> {
    let start_time = Instant::now();

    // Find JSONL file
    let jsonl_path = if let Some(ref path) = file {
        let p = std::path::PathBuf::from(path);
        if !p.exists() {
            anyhow::bail!("File tidak ditemukan: {}", p.display());
        }
        p
    } else {
        let data_dir = PathBuf::from("data");
        if !data_dir.exists() {
            anyhow::bail!("Directory 'data' tidak ditemukan. Jalankan 'full-fetch' dulu.");
        }
        match jsonl::find_latest_jsonl(&data_dir) {
            Some(p) => {
                println!("[AUTO] Menggunakan file terbaru: {}", p.display());
                p
            }
            None => {
                anyhow::bail!("Tidak ada file JSONL di data/. Jalankan 'full-fetch' dulu.");
            }
        }
    };

    // Count lines
    let total_lines = jsonl::count_jsonl(&jsonl_path);
    println!("[JSONL] {} ({} entries)", jsonl_path.display(), total_lines);
    if total_lines == 0 {
        println!("[JSONL] File kosong, tidak ada yang diupload.");
        return Ok(());
    }

    // Connect DB
    let pool = db_connect_from_env().await?;

    // Setup schema
    println!("[DB] Ensuring schema...");
    db::setup_schema(&pool).await?;
    db::ensure_schema(&pool).await?;
    println!("[DB] Schema OK!");

    // Read and batch upload
    println!("{}", "=".repeat(70));
    println!("  UPLOAD DB: {} → Supabase", jsonl_path.display());
    println!("  Batch size: {}", batch_size);
    println!("  Total entries: {}", total_lines);
    println!("{}", "=".repeat(70));

    let jsonl_file = std::fs::File::open(&jsonl_path)
        .with_context(|| format!("Failed to open JSONL: {}", jsonl_path.display()))?;
    let reader = std::io::BufReader::new(jsonl_file);

    let mut db_n: usize = 0;
    let mut db_u: usize = 0;
    let mut parse_skip = 0usize;
    let mut komik_batch: Vec<KomikDetail> = Vec::with_capacity(batch_size);
    let mut processed = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        match serde_json::from_str::<KomikDetail>(trimmed) {
            Ok(detail) => komik_batch.push(detail),
            Err(_) => parse_skip += 1,
        }

        processed += 1;

        // Flush batch
        if komik_batch.len() >= batch_size {
            match db::batch_write_komik(&pool, &komik_batch).await {
                Ok(result) => {
                    db_n += result.new_count;
                    db_u += result.updated_count;
                }
                Err(e) => {
                    eprintln!("  [DB ERR] batch write: {:#}", e);
                    // Fallback: write one by one
                    for detail in &komik_batch {
                        if let Ok(wr) = db::write_komik(&pool, detail).await {
                            if wr.is_new { db_n += 1; } else { db_u += 1; }
                        }
                    }
                }
            }

            let total_done = db_n + db_u;
            eprintln!("  [DB] Uploaded {} komik ({} new, {} updated) | batch progress: {}/{}",
                total_done, db_n, db_u, processed, total_lines);

            komik_batch.clear();
        }
    }

    // Flush remaining
    if !komik_batch.is_empty() {
        match db::batch_write_komik(&pool, &komik_batch).await {
            Ok(result) => {
                db_n += result.new_count;
                db_u += result.updated_count;
            }
            Err(e) => {
                eprintln!("  [DB ERR] final batch: {:#}", e);
                for detail in &komik_batch {
                    if let Ok(wr) = db::write_komik(&pool, detail).await {
                        if wr.is_new { db_n += 1; } else { db_u += 1; }
                    }
                }
            }
        }
        komik_batch.clear();
    }

    let elapsed = start_time.elapsed().as_secs_f64();

    println!();
    println!("{}", "=".repeat(70));
    println!("  UPLOAD DB COMPLETE");
    println!("  Total new:      {}", db_n);
    println!("  Total updated:  {}", db_u);
    println!("  Parse skipped:  {}", parse_skip);
    println!("  Time:           {:.1}s ({:.1} min)", elapsed, elapsed / 60.0);
    if db_n + db_u > 0 {
        println!("  Rate:           {:.0} komik/min",
            (db_n + db_u) as f64 / elapsed.max(0.01) * 60.0);
    }
    println!("{}", "=".repeat(70));

    // Log ke scrape_log
    let _ = db::log_scrape(
        &pool, "upload_db", "completed",
        (db_n + db_u) as i32, db_n as i32, db_u as i32, parse_skip as i32,
        Some(&format!("source={}, batch_size={}, time={:.1}s",
            jsonl_path.display(), batch_size, elapsed)),
    ).await;

    Ok(())
}

// ============================================================
// HELPERS
// ============================================================

fn create_jsonl_path(data_dir: &std::path::Path, dt: &DateTime<Local>) -> PathBuf {
    let timestamp = dt.format("%Y%m%d_%H%M%S").to_string();
    data_dir.join(format!("full_fetch_{timestamp}.jsonl"))
}
