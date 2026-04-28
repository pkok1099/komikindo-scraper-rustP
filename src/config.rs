/// Konfigurasi untuk KomikIndo Scraper.
/// Semua konstanta dan dictionary terpusat di sini.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;

// ============================================================
// BASE URL
// ============================================================
pub const BASE_URL: &str = "https://komikindo.ch";

// ============================================================
// ENVIRONMENT VARIABLES
// ============================================================

/// Stores debug info about .env loading (paths searched, which one was found).
/// Only populated after first access to env_config().
static ENV_LOAD_INFO: Mutex<Option<EnvLoadInfo>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct EnvLoadInfo {
    /// All paths that were searched for .env
    pub searched_paths: Vec<String>,
    /// The path that was successfully loaded (None if no .env found)
    pub loaded_path: Option<String>,
    /// Whether DATABASE_URL was found after all loading attempts
    pub has_database_url: bool,
}

/// Find and load .env from multiple locations.
/// Returns (env file path if found, list of searched paths).
fn find_and_load_env(explicit_path: Option<&str>) -> (Option<String>, Vec<String>) {
    let mut searched = Vec::new();

    // 1. Explicit path from --env flag
    if let Some(p) = explicit_path {
        let path = PathBuf::from(p);
        searched.push(format!("(explicit) {}", path.display()));
        if path.exists() {
            match dotenvy::from_path(&path) {
                Ok(_) => return (Some(path.display().to_string()), searched),
                Err(e) => eprintln!("[ENV] Failed to load {}: {e}", path.display()),
            }
        }
    }

    // 2. Current working directory
    if let Ok(cwd) = env::current_dir() {
        let p = cwd.join(".env");
        searched.push(p.display().to_string());
        if dotenvy::from_path(&p).is_ok() {
            return (Some(p.display().to_string()), searched);
        }
    }

    // 3. Binary's directory
    if let Ok(exe) = env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let p = exe_dir.join(".env");
            searched.push(p.display().to_string());
            if dotenvy::from_path(&p).is_ok() {
                return (Some(p.display().to_string()), searched);
            }
        }
    }

    // 4. Walk up to 5 parent directories from binary
    if let Ok(exe) = env::current_exe() {
        if let Some(start) = exe.parent() {
            let mut dir = start.to_path_buf();
            for _ in 0..5 {
                if let Some(parent) = dir.parent() {
                    dir = parent.to_path_buf();
                    let p = dir.join(".env");
                    searched.push(p.display().to_string());
                    if dotenvy::from_path(&p).is_ok() {
                        return (Some(p.display().to_string()), searched);
                    }
                } else {
                    break;
                }
            }
        }
    }

    // 5. Walk up to 3 parent directories from cwd
    if let Ok(cwd) = env::current_dir() {
        let mut dir = cwd;
        for _ in 0..3 {
            if let Some(parent) = dir.parent() {
                dir = parent.to_path_buf();
                let p = dir.join(".env");
                searched.push(p.display().to_string());
                if dotenvy::from_path(&p).is_ok() {
                    return (Some(p.display().to_string()), searched);
                }
            } else {
                break;
            }
        }
    }

    (None, searched)
}

/// Set the explicit .env path (from --env CLI flag).
/// Must be called BEFORE first access to env_config().
pub fn set_explicit_env_path(path: Option<String>) {
    if let Ok(mut info) = ENV_LOAD_INFO.lock() {
        *info = Some(EnvLoadInfo {
            searched_paths: vec![],
            loaded_path: None,
            has_database_url: false,
        });
    }
    // Get length before consuming path
    let len = path.as_ref().map(|p| p.len()).unwrap_or(0);
    EXPLICIT_ENV_LEN.store(len, std::sync::atomic::Ordering::Relaxed);
    EXPLICIT_ENV_PATH.store(
        path.unwrap_or_default().leak().as_ptr() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

// Thread-safe storage for explicit env path (set before LazyLock init)
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
static EXPLICIT_ENV_PATH: AtomicU64 = AtomicU64::new(0);
static EXPLICIT_ENV_LEN: AtomicUsize = AtomicUsize::new(0);

/// Get the explicit env path as a string (unsafe but only used during init).
fn get_explicit_env_path() -> Option<String> {
    let len = EXPLICIT_ENV_LEN.load(AtomicOrdering::Relaxed);
    if len == 0 {
        return None;
    }
    let ptr = EXPLICIT_ENV_PATH.load(AtomicOrdering::Relaxed) as *const u8;
    // Safety: this is only called during LazyLock init, before any concurrent access.
    // The string was leaked and won't be freed.
    unsafe {
        let slice = std::slice::from_raw_parts(ptr, len);
        Some(String::from_utf8_lossy(slice).to_string())
    }
}

static ENV: LazyLock<EnvConfig> = LazyLock::new(|| {
    let explicit = get_explicit_env_path();
    let (env_path, searched) = find_and_load_env(explicit.as_deref());
    let has_db = env::var("DATABASE_URL").is_ok();

    if let Ok(mut info) = ENV_LOAD_INFO.lock() {
        *info = Some(EnvLoadInfo {
            searched_paths: searched,
            loaded_path: env_path.clone(),
            has_database_url: has_db,
        });
    }

    // Show .env status
    if let Some(ref p) = env_path {
        eprintln!("[ENV] Loaded .env from: {p}");
    } else {
        eprintln!("[ENV] No .env file found — using environment variables only");
        eprintln!("[ENV] Tip: use --env /path/to/.env to specify .env location");
    }

    // DEBUG: show which DATABASE_URL is loaded
    let db_url = env::var("DATABASE_URL").unwrap_or_default();
    if db_url.is_empty() {
        eprintln!("[ENV] DATABASE_URL not set — DB mode disabled");
    } else if db_url.starts_with("file:") {
        eprintln!("[ENV] WARNING: DATABASE_URL looks like SQLite (file:), expected PostgreSQL!");
    }

    EnvConfig {
        database_url: db_url,
        proxy_url: env::var("PROXY_URL").unwrap_or_default(),
        proxy_enabled: matches!(
            env::var("PROXY_ENABLED").unwrap_or_default().to_lowercase().as_str(),
            "1" | "true" | "yes"
        ),
        scraper_retries: env::var("SCRAPER_RETRIES")
            .unwrap_or_else(|_| "3".into())
            .parse()
            .unwrap_or(3),
        scraper_timeout: env::var("SCRAPER_TIMEOUT")
            .unwrap_or_else(|_| "30".into())
            .parse()
            .unwrap_or(30),
    }
});

#[derive(Debug, Clone)]
pub struct EnvConfig {
    /// Supabase PostgreSQL connection string.
    /// Kosong = DB mode disabled (fallback ke JSONL saja).
    pub database_url: String,
    pub proxy_url: String,
    pub proxy_enabled: bool,
    pub scraper_retries: u32,
    #[allow(dead_code)]
    pub scraper_timeout: u64,
}

pub fn env_config() -> &'static EnvConfig {
    &ENV
}

/// Get debug info about .env loading (paths searched, which was loaded).
pub fn env_load_info() -> EnvLoadInfo {
    ENV_LOAD_INFO.lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or(EnvLoadInfo {
            searched_paths: vec!["(not initialized yet)".to_string()],
            loaded_path: None,
            has_database_url: false,
        })
}

/// Mask DATABASE_URL for safe display (hide password).
/// postgresql://user:***@host:port/db
pub fn mask_database_url(url: &str) -> String {
    if url.is_empty() {
        return "(not set)".to_string();
    }
    if url.starts_with("file:") {
        return format!("{}... (SQLite — WARNING)", &url[..url.len().min(30)]);
    }
    // postgresql://user:password@host:port/db
    if let Some(at_pos) = url.find('@') {
        let prefix = &url[..at_pos + 1]; // "postgresql://user:"
        if let Some(colon_pos) = prefix.rfind(':') {
            let user_part = &prefix[..colon_pos + 1]; // "postgresql://user:"
            let masked = format!("{}***{}", user_part, &url[at_pos..]);
            return masked;
        }
    }
    url.to_string()
}

// ============================================================
// URL BUILDERS
// ============================================================

/// Build komik detail page URL dari slug.
/// Contoh: "155895-nano-machine" -> "https://komikindo.ch/komik/155895-nano-machine/"
pub fn build_komik_url(slug: &str) -> String {
    format!("{BASE_URL}/komik/{slug}/")
}

/// Build chapter read page URL dari slug dan chapter number.
/// Catatan: Fungsi ini SEKARANG HANYA dipakai sebagai fallback.
/// Preferensi utama: gunakan chapter_url langsung dari hasil parse detail.
#[allow(dead_code)]
pub fn build_chapter_url(slug: &str, chapter_number: f64) -> String {
    // Format: no trailing .0 untuk bilangan bulat
    let num_str = if chapter_number.fract() == 0.0 {
        format!("{}", chapter_number as i64)
    } else {
        format!("{chapter_number}")
    };
    format!("{BASE_URL}/{slug}-chapter-{num_str}/")
}

// ============================================================
// CDN DOMAINS
// ============================================================

/// domain name -> id (DB 1-based: komikindo=1, imageainewgeneration=2, ...)
pub fn cdn_domain_to_id(domain: &str) -> Option<i16> {
    match domain {
        "komikindo.ch" => Some(1),
        "imageainewgeneration.lol" => Some(2),
        "himmga.lat" => Some(3),
        "gaimgame.pics" => Some(4),
        _ => None,
    }
}

/// id -> base URL (DB 1-based)
pub fn cdn_id_to_base_url(id: i16) -> &'static str {
    match id {
        1 => "https://komikindo.ch",
        2 => "https://imageainewgeneration.lol",
        3 => "https://himmga.lat",
        4 => "https://gaimgame.pics",
        _ => "",
    }
}

// ============================================================
// STATUS MAP
// ============================================================

pub fn status_to_id(status: &str) -> Option<i16> {
    match status.to_lowercase().as_str() {
        "berjalan" => Some(1),
        "tamat" => Some(2),
        _ => None,
    }
}

// ============================================================
// IMAGE EXTENSIONS
// ============================================================

pub fn ext_to_id(ext: &str) -> i16 {
    match ext.to_lowercase().as_str() {
        ".jpg" => 1,
        ".jpeg" => 2,
        ".png" => 3,
        ".webp" => 4,
        ".gif" => 5,
        _ => 0,
    }
}

pub fn id_to_ext(id: i16) -> &'static str {
    match id {
        1 => ".jpg",
        2 => ".jpeg",
        3 => ".png",
        4 => ".webp",
        5 => ".gif",
        _ => "",
    }
}

// ============================================================
// GENRE MAP (82 hardcoded dari komikindo.ch/daftar-manga/)
// ============================================================

pub fn build_genre_map() -> HashMap<&'static str, i16> {
    let mut m = HashMap::new();
    m.insert("Action", 1);
    m.insert("Adult", 2);
    m.insert("Adventure", 3);
    m.insert("Aliens", 4);
    m.insert("Animals", 5);
    m.insert("Arts", 6);
    m.insert("Boys' Love", 7);
    m.insert("Comedy", 8);
    m.insert("Cooking", 9);
    m.insert("Crime", 10);
    m.insert("Crossdressing", 11);
    m.insert("Delinquents", 12);
    m.insert("Demons", 13);
    m.insert("Drama", 14);
    m.insert("Drama Supernatural", 15);
    m.insert("Ecchi", 16);
    m.insert("Fantasy", 17);
    m.insert("Gender Bender", 18);
    m.insert("Genderswap", 19);
    m.insert("Ghosts", 20);
    m.insert("Girls' Love", 21);
    m.insert("Gore", 22);
    m.insert("Gyaru", 23);
    m.insert("Harem", 24);
    m.insert("Historical", 25);
    m.insert("Horror", 26);
    m.insert("Incest", 27);
    m.insert("Isekai", 28);
    m.insert("Josei", 29);
    m.insert("Life", 30);
    m.insert("Loli", 31);
    m.insert("Mafia", 32);
    m.insert("Magic", 33);
    m.insert("Magical Girls", 34);
    m.insert("Martial", 35);
    m.insert("Martial Arts", 36);
    m.insert("Mature", 37);
    m.insert("Mecha", 38);
    m.insert("Medical", 39);
    m.insert("Military", 40);
    m.insert("Monster Girls", 41);
    m.insert("Monsters", 42);
    m.insert("Music", 43);
    m.insert("Mystery", 44);
    m.insert("Ninja", 45);
    m.insert("Office Workers", 46);
    m.insert("Philosophical", 47);
    m.insert("Police", 48);
    m.insert("Post-Apocalyptic", 49);
    m.insert("Psychological", 50);
    m.insert("Reincarnation", 51);
    m.insert("Reverse Harem", 52);
    m.insert("Romance", 53);
    m.insert("Samurai", 54);
    m.insert("School", 55);
    m.insert("School Life", 56);
    m.insert("Sci-Fi", 57);
    m.insert("Seinen", 58);
    m.insert("Sexual Violence", 59);
    m.insert("Shota", 60);
    m.insert("Shoujo", 61);
    m.insert("Shoujo Ai", 62);
    m.insert("Shounen", 63);
    m.insert("Shounen Ai", 64);
    m.insert("Slice of Life", 65);
    m.insert("Smut", 66);
    m.insert("Sports", 67);
    m.insert("Superhero", 68);
    m.insert("Supernatural", 69);
    m.insert("Survival", 70);
    m.insert("Thriller", 71);
    m.insert("Time Travel", 72);
    m.insert("Traditional Games", 73);
    m.insert("Traged", 74);
    m.insert("Tragedy", 75);
    m.insert("Vampires", 76);
    m.insert("Video Games", 77);
    m.insert("Villainess", 78);
    m.insert("Virtual Reality", 79);
    m.insert("Wuxia", 80);
    m.insert("Yuri", 81);
    m.insert("Zombies", 82);
    m
}

pub fn build_genre_names() -> HashMap<i16, &'static str> {
    let m = build_genre_map();
    m.into_iter().map(|(k, v)| (v, k)).collect()
}

// ============================================================
// THUMBNAIL CONSTANTS
// ============================================================
pub const THUMB_WP_PREFIX: &str = "wp-content/uploads/";
pub const THUMB_KOMIK_PREFIX: &str = "Komik-";
