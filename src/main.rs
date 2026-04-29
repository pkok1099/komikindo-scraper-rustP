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

mod config;
mod db;
mod fetcher;
mod jsonl;
mod parsers;
mod scraper;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use clap::Parser;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use sqlx::PgPool;
use sqlx::Row;

use crate::fetcher::Fetcher;
use crate::scraper::{scrape_full_komik_list, scrape_komik_detail, scrape_komik_terbaru};
use crate::config::BASE_URL;
use crate::parsers::KomikDetail;

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
    #[arg(long, global = true, default_value_t = 256)]
    max_blocking_threads: usize,

    /// Max in-flight HTTP requests (default: 256).
    #[arg(long, global = true, default_value_t = 256)]
    max_in_flight: usize,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Full fetch semua komik + detail (FULL SPEED)
    /// Method 2: tanpa image scraping.
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

        /// Batch size untuk DB insert (default: 500)
        #[arg(long, default_value_t = 500)]
        batch_size: usize,
    },

    /// Drop all scraper tables (DANGEROUS): removes tables completely
    DbDropAll,
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
// FULL FETCH (Method 2: tanpa image, dengan optional DB)
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

    println!("{}", "=".repeat(70));
    println!("  KOMIKINDO FULL FETCH → JSONL (no DB)");
    println!("  Started at {}", dt_start.format("%Y-%m-%d %H:%M:%S"));
    println!("  Timeout: {}s", opts.timeout);
    println!("  Mode: Method 2 (tanpa image)");
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

    // Spawn in larger chunks for better pipelining
    const FETCH_CHUNK: usize = 200;
    let total_chunks = (total + FETCH_CHUNK - 1) / FETCH_CHUNK;
    println!("[INFO] {} chunks of max {} komik", total_chunks, FETCH_CHUNK);

    // Use buffered JSONL writer (keeps file open, 256KB buffer)
    let buffered_writer = Arc::new(jsonl::BufferedJsonlWriter::new(&jsonl_path)?);
    let bw = Arc::clone(&buffered_writer);

    let write_success = Arc::clone(&success);
    let write_failed = Arc::clone(&failed);
    let write_ch = Arc::clone(&total_chapters);

    let fetch_task = tokio::spawn(async move {
        for (chunk_idx, chunk) in komik_list.chunks(FETCH_CHUNK).enumerate() {
            if chunk_idx > 0 {
                eprintln!("  [FETCH] Chunk {}/{} ({} komik)...",
                    chunk_idx + 1, total_chunks, chunk.len());
            }

            let mut handles = Vec::with_capacity(chunk.len());
            for slug in chunk {
                let fetcher = Arc::clone(&writer_fetcher);
                let slug = slug.clone();
                handles.push(tokio::spawn(async move {
                    let result = scrape_komik_detail(&slug, &fetcher).await;
                    (slug, result)
                }));
            }

            for handle in handles {
                match handle.await {
                    Ok((slug, result)) => {
                        match result {
                            Ok(detail) => {
                                let ch_count = detail.chapters.len();
                                write_ch.fetch_add(ch_count, Ordering::Relaxed);

                                // Write to JSONL using buffered writer (fast)
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
            }
        }
    });

    // Progress reporter (Phase 1 only)
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
                "  [FETCH {}/{} {:.1}%] {:.0} komik/min | ETA: {:.0}min | \
                 OK: {} FAIL: {} | Ch: {} | DL: {:.1}MB | req: {}\r",
                c, progress_total, c as f64 / progress_total as f64 * 100.0,
                rate, eta, s, f,
                progress_ch.load(Ordering::Relaxed),
                stats.mb_downloaded(), stats.requests,
            );
        }
    });

    // Wait for fetch to complete
    let _ = fetch_task.await;
    progress_handle.abort();

    // Flush buffered JSONL writer to ensure all data is on disk
    buffered_writer.flush()?;

    let fetch_elapsed = start_time.elapsed().as_secs_f64();
    let stats = fetcher.stats();
    let s = success.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let ch = total_chapters.load(Ordering::Relaxed);

    println!();
    println!("  [FETCH DONE] {s} komik, {f} failed, {ch} chapters in {:.1}s ({:.1} min)",
        fetch_elapsed, fetch_elapsed / 60.0);
    println!("  [FETCH RATE] {:.1} komik/min", s as f64 / fetch_elapsed * 60.0);

    let jsonl_size_mb = std::fs::metadata(jsonl_path.as_ref())
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);
    println!("  [JSONL] {} ({:.1} MB)", jsonl_path.display(), jsonl_size_mb);

    // === Summary ===
    let total_elapsed = start_time.elapsed().as_secs_f64();

    println!();
    println!("{}", "=".repeat(70));
    println!("  FULL FETCH COMPLETE");
    println!("  Total komik:    {total}");
    println!("  Success:        {s}");
    println!("  Failed:         {f}");
    println!("  Chapters:       {ch}");
    println!("  Fetch time:     {:.1}s ({:.1}min) | {:.1} komik/min",
        fetch_elapsed, fetch_elapsed / 60.0,
        s as f64 / fetch_elapsed * 60.0);
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

    // === Step 5: Update latest_chapter_number in DB for chapter-only updates ===
    if !opts.dry_run {
        if let Some((pool, _)) = &db_pool {
            if !updated_chapters.is_empty() {
                println!("\n--- Step 5: Updating latest_chapter_number in DB ---");
                let mut db_ch_updated = 0usize;
                for (slug, _old_ch, new_ch, _gap) in &updated_chapters {
                    if let Some(&(komik_id, _)) = db_chapter_map.get(slug) {
                        match db::update_latest_chapter(pool, komik_id, *new_ch).await {
                            Ok(true) => {
                                db_ch_updated += 1;
                            }
                            Ok(false) => {
                                // Already up to date (concurrent update)
                            }
                            Err(e) => {
                                eprintln!("  [DB WARN] Failed to update {slug}: {e}");
                            }
                        }
                    }
                }
                println!("[DB] Updated latest_chapter_number for {db_ch_updated} komik");
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
