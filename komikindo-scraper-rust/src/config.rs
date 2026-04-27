/// Konfigurasi untuk KomikIndo Scraper.
/// Semua konstanta dan dictionary terpusat di sini.

use std::collections::HashMap;
use std::env;
use std::sync::LazyLock;

// ============================================================
// BASE URL
// ============================================================
pub const BASE_URL: &str = "https://komikindo.ch";

// ============================================================
// ENVIRONMENT VARIABLES
// ============================================================

static ENV: LazyLock<EnvConfig> = LazyLock::new(|| {
    dotenvy::dotenv().ok();
    EnvConfig {
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
    pub proxy_url: String,
    pub proxy_enabled: bool,
    pub scraper_retries: u32,
    pub scraper_timeout: u64,
}

pub fn env_config() -> &'static EnvConfig {
    &ENV
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

/// domain name -> id
pub fn cdn_domain_to_id(domain: &str) -> Option<i16> {
    match domain {
        "komikindo.ch" => Some(0),
        "imageainewgeneration.lol" => Some(1),
        "himmga.lat" => Some(2),
        "gaimgame.pics" => Some(3),
        _ => None,
    }
}

/// id -> base URL
pub fn cdn_id_to_base_url(id: i16) -> &'static str {
    match id {
        0 => "https://komikindo.ch",
        1 => "https://imageainewgeneration.lol",
        2 => "https://himmga.lat",
        3 => "https://gaimgame.pics",
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
