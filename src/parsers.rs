/// HTML parsing functions.
/// CPU-bound parsing menggunakan `scraper` crate.
///
/// PERBAIKAN SLUG: Chapter URL diambil langsung dari hasil parse detail page,
/// BUKAN di-construct manual dari slug + chapter_number.

use crate::config::*;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

// ============================================================
// CACHED SELECTORS / REGEX (avoid re-parse per page)
// ============================================================

static RE_THUMB_DIM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d+x\d+\.").unwrap());
static RE_SLUG_FROM_KOMIK_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/komik/([^/]+)/?").unwrap());

static SEL_KOMIK_LIST_PRIMARY: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.tip").unwrap());
static SEL_KOMIK_LIST_FALLBACK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href*='/komik/']").unwrap());

// ============================================================
// URL NORMALIZATION
// ============================================================

/// Strip domain dari full URL, return (domain_id, path).
pub fn normalize_url(full_url: &str) -> Option<(i16, String)> {
    if let Some(pos) = full_url.find("://") {
        let after = &full_url[pos + 3..];
        let slash_pos = after.find('/')?;
        let domain = &after[..slash_pos];
        let path = &after[slash_pos + 1..];

        if let Some(domain_id) = cdn_domain_to_id(domain) {
            return Some((domain_id, path.to_string()));
        }
    }
    None
}

/// Reconstruct full URL dari domain_id dan path.
#[allow(dead_code)]
pub fn reconstruct_url(domain_id: i16, path: &str) -> String {
    let base = cdn_id_to_base_url(domain_id);
    format!("{base}/{path}")
}

/// Remove -NNNxNNN dimension suffix dari thumbnail URL/path.
pub fn strip_thumbnail_dimensions(s: &str) -> String {
    RE_THUMB_DIM.replace(s, ".").to_string()
}

/// Normalize thumbnail: domain ref + strip wp prefix + Komik- + dimensions.
pub fn normalize_thumbnail(full_url: &str) -> Option<(i16, String)> {
    let (domain_id, mut path) = normalize_url(full_url)?;

    // Strip wp-content/uploads/ prefix
    if path.starts_with(THUMB_WP_PREFIX) {
        path = path[THUMB_WP_PREFIX.len()..].to_string();
    }

    // Strip -NNNxNNN dimension suffix
    path = strip_thumbnail_dimensions(&path);

    // Strip Komik- filename prefix
    if let Some(last_slash) = path.rfind('/') {
        let dir_part = &path[..=last_slash];
        let filename = &path[last_slash + 1..];
        if filename.starts_with(THUMB_KOMIK_PREFIX) {
            path = format!("{}{}", dir_part, &filename[THUMB_KOMIK_PREFIX.len()..]);
        }
    } else if path.starts_with(THUMB_KOMIK_PREFIX) {
        path = path[THUMB_KOMIK_PREFIX.len()..].to_string();
    }

    Some((domain_id, path))
}

// ============================================================
// PARSERS
// ============================================================

/// Parse komik list page HTML -> list of slugs.
pub fn parse_komik_list(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let mut slugs = Vec::new();
    let mut seen = HashSet::new();

    // Try primary selector first, fallback if empty.
    let mut iter = document.select(&SEL_KOMIK_LIST_PRIMARY);
    let primary_empty = iter.next().is_none();

    let links: Box<dyn Iterator<Item = ElementRef>> = if primary_empty {
        Box::new(document.select(&SEL_KOMIK_LIST_FALLBACK))
    } else {
        // Restart iterator (we consumed one item)
        Box::new(document.select(&SEL_KOMIK_LIST_PRIMARY))
    };

    for link in links {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !href.contains("/komik/") {
            continue;
        }
        if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(href) {
            let slug = caps[1].to_string();
            if !slug.is_empty() && seen.insert(slug.clone()) {
                slugs.push(slug);
            }
        }
    }

    slugs
}

/// Parse komik detail page HTML -> detail dict.
///
/// PERBAIKAN: chapter URL diambil langsung dari `<a href="...">` di detail page,
/// sehingga tidak perlu construct manual dari slug.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct KomikDetail {
    pub slug: String,
    pub judul: Option<String>,
    pub tipe: Option<String>,
    pub thumb_domain_id: Option<i16>,
    pub thumb_path: Option<String>,
    pub status_id: Option<i16>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub alternative_title: Option<String>,
    pub sinopsis: Option<String>,
    pub rating: Option<f64>,
    pub genre_list: Vec<String>,
    pub genre_ids: Vec<i16>,
    /// Chapter list - SUDAH TERMASUK URL langsung dari detail page
    pub chapters: Vec<ChapterInfo>,
    pub latest_chapter_number: Option<f64>,
}

/// Chapter info dari detail page.
/// `url` diambil langsung dari href di halaman detail - TIDAK di-construct.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChapterInfo {
    pub number: f64,
    /// URL langsung dari halaman detail komik (e.g. "https://komikindo.ch/nano-machine-chapter-309/")
    /// INI adalah fix untuk masalah slug - tidak perlu lagi construct manual!
    pub url: String,
    // Data image (diisi nanti saat scrape chapter)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_domain_id: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_path_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_filenames: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_ext_ids: Option<Vec<i16>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_images: Option<i32>,
}

impl ChapterInfo {
    #[allow(dead_code)]
    pub fn set_image_data(&mut self, data: ChapterImageData) {
        self.cdn_domain_id = Some(data.cdn_domain_id);
        self.cdn_path_prefix = Some(data.cdn_path_prefix);
        self.image_filenames = Some(data.image_filenames);
        self.image_ext_ids = Some(data.image_ext_ids);
        self.total_images = Some(data.total_images as i32);
    }
}

/// Optimized image data dari chapter read page.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ChapterImageData {
    pub cdn_domain_id: i16,
    pub cdn_path_prefix: String,
    pub image_filenames: Vec<String>,
    pub image_ext_ids: Vec<i16>,
    pub total_images: usize,
}

#[allow(dead_code)]
pub fn parse_komik_detail(slug: &str, html: &str) -> Option<KomikDetail> {
    let document = Html::parse_document(html);
    let genre_map = build_genre_map();

    let mut detail = KomikDetail {
        slug: slug.to_string(),
        judul: None,
        tipe: None,
        thumb_domain_id: None,
        thumb_path: None,
        status_id: None,
        author: None,
        artist: None,
        alternative_title: None,
        sinopsis: None,
        rating: None,
        genre_list: Vec::new(),
        genre_ids: Vec::new(),
        chapters: Vec::new(),
        latest_chapter_number: None,
    };

    // === TITLE ===
    let title_selectors = [
        "h1.titless",
        "h1.entry-title",
        "div.infox h1",
    ];
    for sel_str in &title_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                let title = el.text().collect::<String>().trim().to_string();
                if !title.is_empty() {
                    let cleaned = if title.to_lowercase().starts_with("komik") {
                        Regex::new(r"(?i)^komik\s*")
                            .unwrap()
                            .replace(&title, "")
                            .trim()
                            .to_string()
                    } else {
                        title
                    };
                    detail.judul = Some(cleaned);
                    break;
                }
            }
        }
    }

    // === TYPE ===
    if let Ok(sel) = Selector::parse("span.typeflag") {
        if let Some(el) = document.select(&sel).next() {
            for cls in el.value().classes() {
                if cls != "typeflag" {
                    detail.tipe = Some(cls.to_string());
                    break;
                }
            }
        }
    }

    // === THUMBNAIL ===
    let thumb_selectors = [
        "div.infoanime img",
        "div.thumb img",
        "div.itemprop img",
    ];
    let mut thumb_url = String::new();
    for sel_str in &thumb_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                if let Some(src) = el.value().attr("src") {
                    thumb_url = src.to_string();
                    break;
                }
            }
        }
    }

    if thumb_url.is_empty() {
        if let Ok(sel) = Selector::parse("img[itemprop='image']") {
            for el in document.select(&sel) {
                if let Some(src) = el.value().attr("src") {
                    if src.to_lowercase().contains("/komik-") {
                        thumb_url = src.to_string();
                        break;
                    }
                }
            }
        }
    }

    if !thumb_url.is_empty() {
        if let Some((domain_id, path)) = normalize_thumbnail(&thumb_url) {
            detail.thumb_domain_id = Some(domain_id);
            detail.thumb_path = Some(path);
        } else {
            detail.thumb_domain_id = Some(0);
            detail.thumb_path = Some(thumb_url);
        }
    }

    // === INFO (status, author, artist, dll) ===
    let info_sel = Selector::parse("div.infox .spe span").unwrap();
    let mut raw_genre_text: Option<String> = None;

    for span in document.select(&info_sel) {
        let bold_sel = Selector::parse("b").unwrap();
        let Some(bold) = span.select(&bold_sel).next() else {
            continue;
        };

        let label = bold.text().collect::<String>().trim().trim_end_matches(':').to_lowercase();
        let full_text = span.text().collect::<String>();
        let bold_text = bold.text().collect::<String>();
        let value = full_text.replace(&bold_text, "").trim().to_string();

        if label.contains("genre") {
            raw_genre_text = Some(value.clone());
        } else if label.contains("status") {
            if let Some(id) = status_to_id(&value) {
                detail.status_id = Some(id);
            }
        } else if label.contains("pengarang") || label.contains("author") {
            detail.author = Some(value);
        } else if label.contains("ilustrator") || label.contains("artist") {
            detail.artist = Some(value);
        } else if label.contains("tipe") || label.contains("type") {
            if detail.tipe.is_none() {
                detail.tipe = Some(value);
            }
        } else if label.contains("alternative") {
            detail.alternative_title = Some(value);
        }
    }

    // Genre dari tag links (lebih reliable)
    let genre_link_sel = Selector::parse("div.genre-info a[rel='tag'], div.infox .spe a[rel='tag']").unwrap();
    let genre_links: Vec<_> = document.select(&genre_link_sel).collect();

    let genre_list: Vec<String> = if !genre_links.is_empty() {
        genre_links
            .iter()
            .map(|a| a.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if let Some(ref text) = raw_genre_text {
        text.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };

    for g in &genre_list {
        if let Some(&gid) = genre_map.get(g.as_str()) {
            detail.genre_ids.push(gid);
        }
    }
    detail.genre_list = genre_list;

    // === RATING ===
    let rating_selectors = ["div.infoanime-rating i", "div.rating i", "span.skor"];
    for sel_str in &rating_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                let r_text = el.text().collect::<String>().trim().to_string();
                if !r_text.is_empty() {
                    if let Ok(r) = r_text.parse::<f64>() {
                        detail.rating = Some(r);
                        break;
                    }
                }
            }
        }
    }

    // === SINOPSIS ===
    let sinopsis_selectors = [
        "div.entry-content.entry-content-single",
        "div.entry-content",
        "div#sinopsis div.entry-content",
    ];
    for sel_str in &sinopsis_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = document.select(&sel).next() {
                let mut sinopsis = el.text().collect::<String>();
                // Bersihkan prefix
                let re_prefix = Regex::new(r"(?is)^Update chapter terbaru komik.*?Sinopsis\s*").unwrap();
                sinopsis = re_prefix.replace(&sinopsis, "").to_string();
                let re_type = Regex::new(r"^(Manga|Manhwa|Manhua)\s+").unwrap();
                sinopsis = re_type.replace(&sinopsis, "").to_string();
                sinopsis = sinopsis.trim().to_string();
                if !sinopsis.is_empty() {
                    detail.sinopsis = Some(sinopsis);
                    break;
                }
            }
        }
    }

    // === CHAPTERS ===
    // PERBAIKAN SLUG: Ambil URL langsung dari href di detail page!
    detail.chapters = extract_chapters(&document);
    if let Some(first) = detail.chapters.first() {
        detail.latest_chapter_number = Some(first.number);
    }

    Some(detail)
}

/// Extract chapter list dari detail page.
///
/// **PERBAIKAN SLUG**: URL chapter diambil langsung dari `<a href="...">`
/// yang ada di halaman detail komik. Tidak perlu lagi construct manual!
fn extract_chapters(document: &Html) -> Vec<ChapterInfo> {
    let mut chapters = Vec::new();
    let mut seen_urls = HashSet::new();

    // Coba dari container utama
    let container_selectors = ["div.bxcl", "div#chapter_list"];
    let mut chapter_links: Vec<ElementRef> = Vec::new();

    for sel_str in &container_selectors {
        if let Ok(container_sel) = Selector::parse(sel_str) {
            if let Some(container) = document.select(&container_sel).next() {
                let link_sel = Selector::parse("span.lchx a, a[href*='-chapter-']").unwrap();
                chapter_links = container.select(&link_sel).collect();
                if !chapter_links.is_empty() {
                    break;
                }
            }
        }
    }

    // Fallback: cari semua link chapter di halaman
    if chapter_links.is_empty() {
        let link_sel = Selector::parse("a[href*='-chapter-']").unwrap();
        chapter_links = document.select(&link_sel).collect();
    }

    for link in &chapter_links {
        let Some(ch_url) = link.value().attr("href") else {
            continue;
        };
        if ch_url.is_empty() {
            continue;
        }

        let full_url = if ch_url.starts_with('/') {
            format!("{BASE_URL}{ch_url}")
        } else {
            ch_url.to_string()
        };

        if !seen_urls.insert(full_url.clone()) {
            continue;
        }

        let text = link.text().collect::<String>();
        let Some(number) = extract_chapter_number(&text, &full_url) else {
            continue;
        };

        chapters.push(ChapterInfo {
            number,
            url: full_url, // URL LANGSUNG dari detail page!
            cdn_domain_id: None,
            cdn_path_prefix: None,
            image_filenames: None,
            image_ext_ids: None,
            total_images: None,
        });
    }

    chapters.sort_by(|a, b| b.number.partial_cmp(&a.number).unwrap_or(std::cmp::Ordering::Equal));
    chapters
}

/// Extract chapter number dari text dan URL.
fn extract_chapter_number(title: &str, url: &str) -> Option<f64> {
    // Coba dari title: "Chapter 309" atau format lainnya
    let re = Regex::new(r"(?i)chapter\s*([\d.]+)").unwrap();
    if let Some(caps) = re.captures(title) {
        if let Ok(num) = caps[1].parse::<f64>() {
            return Some(num);
        }
    }

    // Coba dari URL: "nano-machine-chapter-309"
    let re_url = Regex::new(r"(?i)chapter-([\d.]+)").unwrap();
    if let Some(caps) = re_url.captures(url) {
        if let Ok(num) = caps[1].parse::<f64>() {
            return Some(num);
        }
    }

    None
}

// ============================================================
// CHAPTER IMAGE PARSER
// ============================================================

/// Parse chapter read page HTML untuk CDN image URLs.
///
/// **PERBAIKAN**: URL chapter sudah dikirim langsung dari detail page,
/// jadi tidak ada lagi masalah slug yang salah!
#[allow(dead_code)]
pub fn parse_chapter_images(html: &str) -> ChapterImageData {
    let document = Html::parse_document(html);
    let mut raw_images = Vec::new();

    // Primary: div#chimg-auh
    let chimg_selectors = ["div#chimg-auh", "div.chimg-auh"];
    for sel_str in &chimg_selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(container) = document.select(&sel).next() {
                let img_sel = Selector::parse("img").unwrap();
                for img in container.select(&img_sel) {
                    if let Some(src) = img.value().attr("src") {
                        if is_chapter_image(src) {
                            raw_images.push(src.to_string());
                        }
                    }
                }
                if !raw_images.is_empty() {
                    break;
                }
            }
        }
    }

    // Fallback: semua img di halaman
    if raw_images.is_empty() {
        let img_sel = Selector::parse("img").unwrap();
        for img in document.select(&img_sel) {
            if let Some(src) = img.value().attr("src") {
                if is_chapter_image(src) {
                    raw_images.push(src.to_string());
                }
            }
        }
    }

    if raw_images.is_empty() {
        return ChapterImageData {
            cdn_domain_id: 0,
            cdn_path_prefix: String::new(),
            image_filenames: Vec::new(),
            image_ext_ids: Vec::new(),
            total_images: 0,
        };
    }

    // Normalize URLs -> (domain_id, path)
    let normalized: Vec<(i16, String)> = raw_images
        .iter()
        .filter_map(|url| normalize_url(url))
        .collect();

    let mut domain_counts: HashMap<i16, usize> = HashMap::new();
    for &(domain_id, _) in &normalized {
        *domain_counts.entry(domain_id).or_insert(0) += 1;
    }

    // Find common prefix
    let all_paths: Vec<&str> = normalized.iter().map(|(_, p)| p.as_str()).collect();
    let mut common_prefix = find_common_prefix(&all_paths);

    // Ensure prefix ends at / boundary
    if let Some(last_slash) = common_prefix.rfind('/') {
        common_prefix = &common_prefix[..last_slash];
    } else {
        common_prefix = "";
    }

    // Primary domain = most common
    let primary_domain = domain_counts
        .into_iter()
        .max_by_key(|&(_, count)| count)
        .map(|(id, _)| id)
        .unwrap_or(0);

    // Build optimized output
    let mut image_filenames = Vec::new();
    let mut image_ext_ids = Vec::new();

    for (_, path) in &normalized {
        let remainder = if !common_prefix.is_empty() && path.starts_with(&format!("{common_prefix}/")) {
            &path[common_prefix.len() + 1..]
        } else {
            path.as_str()
        };

        // Split filename dan extension
        if let Some(dot_pos) = remainder.rfind('.') {
            let name = &remainder[..dot_pos];
            let ext = &remainder[dot_pos..].to_lowercase();
            image_filenames.push(name.to_string());
            image_ext_ids.push(ext_to_id(ext));
        } else {
            image_filenames.push(remainder.to_string());
            image_ext_ids.push(0);
        }
    }

    ChapterImageData {
        cdn_domain_id: primary_domain,
        cdn_path_prefix: common_prefix.to_string(),
        total_images: image_filenames.len(),
        image_filenames,
        image_ext_ids,
    }
}

/// Cek apakah URL adalah chapter image (bukan ads/logo).
#[allow(dead_code)]
fn is_chapter_image(url: &str) -> bool {
    let url_lower = url.to_lowercase();

    let cdn_patterns = [
        "imageainewgeneration.lol/data/",
        "himmga.lat/data/",
        "gaimgame.pics/data/",
        "imageainewgeneration",
        "/data/",
    ];

    let is_cdn = cdn_patterns.iter().any(|p| url_lower.contains(p));

    let exclude_patterns = [
        "fav.png",
        "favicon",
        "logo",
        "ads",
        "banner",
        ".gif",
        "button",
        "icon",
        "gravatar",
        "emoji",
    ];

    let is_excluded = exclude_patterns.iter().any(|p| url_lower.contains(p));

    is_cdn && !is_excluded
}

// ============================================================
// HOMEPAGE PARSER
// ============================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HomepageUpdate {
    pub slug: String,
    pub judul: Option<String>,
    pub latest_chapter_number: Option<f64>,
    pub tipe: Option<String>,
    pub thumbnail: Option<String>,
}

static RE_CHAPTER_FROM_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());
static SEL_HOME_POST_PRIMARY: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.listupd div.animepost").unwrap());
static SEL_HOME_POST_FALLBACK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.listupd div.bsx, div.listupd div.bs div.bsx").unwrap());
static SEL_HOME_TITLE_LINK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.bigors div.tt h3 a, h3 a, a[href*='/komik/']").unwrap());
static SEL_HOME_CH_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("div.bigors div.adds a[href*='chapter'], a[href*='-chapter-']").unwrap()
});
static SEL_HOME_IMG: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.limit img, img").unwrap());
static SEL_HOME_TYPEFLAG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span.typeflag").unwrap());

pub fn parse_homepage_updates(html: &str) -> Vec<HomepageUpdate> {
    let document = Html::parse_document(html);
    let mut updates = Vec::new();
    let mut seen = HashSet::new();

    // Primary: div.listupd div.animepost
    let mut iter = document.select(&SEL_HOME_POST_PRIMARY);
    let primary_empty = iter.next().is_none();
    let posts: Box<dyn Iterator<Item = ElementRef>> = if primary_empty {
        Box::new(document.select(&SEL_HOME_POST_FALLBACK))
    } else {
        Box::new(document.select(&SEL_HOME_POST_PRIMARY))
    };

    for post in posts {
        let mut item = HomepageUpdate {
            slug: String::new(),
            judul: None,
            latest_chapter_number: None,
            tipe: None,
            thumbnail: None,
        };

        // Title link
        if let Some(title_link) = post.select(&SEL_HOME_TITLE_LINK).next() {
            item.judul = Some(
                title_link
                    .text()
                    .collect::<String>()
                    .trim()
                    .to_string(),
            );
            if let Some(href) = title_link.value().attr("href") {
                let full_href = if href.starts_with('/') {
                    format!("{BASE_URL}{href}")
                } else {
                    href.to_string()
                };
                if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(&full_href) {
                    item.slug = caps[1].to_string();
                }
            }
        }

        if item.slug.is_empty() {
            continue;
        }

        if !seen.insert(item.slug.clone()) {
            continue;
        }

        // Chapter link
        if let Some(ch_link) = post.select(&SEL_HOME_CH_LINK).next() {
            if let Some(href) = ch_link.value().attr("href") {
                let full_href = if href.starts_with('/') {
                    format!("{BASE_URL}{href}")
                } else {
                    href.to_string()
                };
                if let Some(caps) = RE_CHAPTER_FROM_URL.captures(&full_href) {
                    if let Ok(num) = caps[1].parse::<f64>() {
                        item.latest_chapter_number = Some(num);
                    }
                }
            }
        }

        // Type
        if let Some(type_el) = post.select(&SEL_HOME_TYPEFLAG).next() {
            for cls in type_el.value().classes() {
                if cls != "typeflag" {
                    item.tipe = Some(cls.to_string());
                    break;
                }
            }
        }

        // Thumbnail
        if let Some(img) = post.select(&SEL_HOME_IMG).next() {
            item.thumbnail = img.value().attr("src").map(|s| s.to_string());
        }

        updates.push(item);
    }

    updates
}

// ============================================================
// KOMIK TERBARU PARSER (/komik-terbaru/)
// ============================================================

/// Item dari halaman /komik-terbaru/.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TerbaruItem {
    /// Slug komik (e.g., "blue-lock", "675026-blue-lock")
    pub slug: String,
    /// Judul komik
    pub judul: String,
    /// Nomor chapter terbaru (e.g., 342.0)
    pub chapter_number: f64,
    /// Full URL chapter terbaru (e.g., "https://komikindo.ch/blue-lock-chapter-342/")
    pub chapter_url: String,
    /// URL base untuk construct chapter URL baru.
    /// Diambil dari chapter_url, stripped "-chapter-{number}".
    /// (e.g., "https://komikindo.ch/blue-lock")
    pub url_base: String,
    /// Time string asli (e.g., "3 menit lalu", "4 jam lalu")
    pub time_str: String,
    /// Time dalam menit untuk sorting/comparison
    pub time_minutes: u32,
}

static SEL_TERBARU_POST: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.animepost").unwrap());
static SEL_TERBARU_KOMIK: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("h3 a[href*='/komik/'], a.animposx[href*='/komik/']").unwrap());
static SEL_TERBARU_CH: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.lsch a[href*='-chapter-']").unwrap());
static SEL_TERBARU_TIME: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("span.datech").unwrap());
static RE_TERBARU_CH_NUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());

static RE_TIME_MIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*menit").unwrap());
static RE_TIME_HOUR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*jam").unwrap());
static RE_TIME_DAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*hari").unwrap());

/// Parse halaman /komik-terbaru/ -> list of TerbaruItem.
pub fn parse_komik_terbaru(html: &str) -> Vec<TerbaruItem> {
    let document = Html::parse_document(html);
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for post in document.select(&SEL_TERBARU_POST) {
        // --- Komik link & slug ---
        let komik_link = match post.select(&SEL_TERBARU_KOMIK).next() {
            Some(el) => el,
            None => continue,
        };
        let komik_href = match komik_link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let slug = match RE_SLUG_FROM_KOMIK_URL.captures(komik_href) {
            Some(caps) => caps[1].to_string(),
            None => continue,
        };
        if slug.is_empty() || !seen.insert(slug.clone()) {
            continue;
        }

        let judul = komik_link.text().collect::<String>().trim().to_string();

        // --- Chapter link ---
        let ch_link = match post.select(&SEL_TERBARU_CH).next() {
            Some(el) => el,
            None => continue,
        };
        let ch_url_raw = match ch_link.value().attr("href") {
            Some(u) => u,
            None => continue,
        };
        let ch_url = normalize_url_full(ch_url_raw);
        let chapter_number = match RE_TERBARU_CH_NUM.captures(&ch_url) {
            Some(caps) => caps[1].parse::<f64>().unwrap_or(0.0),
            None => continue,
        };

        // --- Time ---
        let time_str = post
            .select(&SEL_TERBARU_TIME)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let time_minutes = parse_time_to_minutes(&time_str);

        // --- URL base (untuk construct chapter baru) ---
        let url_base = extract_url_base(&ch_url);

        items.push(TerbaruItem {
            slug,
            judul,
            chapter_number,
            chapter_url: ch_url,
            url_base,
            time_str,
            time_minutes,
        });
    }

    items
}

/// Parse time string "X menit lalu" / "X jam lalu" / "X hari lalu" -> menit.
fn parse_time_to_minutes(time_str: &str) -> u32 {
    let s = time_str.trim().to_lowercase();

    if let Some(m) = RE_TIME_MIN.captures(&s) {
        return m[1].parse::<u32>().unwrap_or(0);
    }
    if let Some(m) = RE_TIME_HOUR.captures(&s) {
        return m[1].parse::<u32>().unwrap_or(0) * 60;
    }
    if let Some(m) = RE_TIME_DAY.captures(&s) {
        return m[1].parse::<u32>().unwrap_or(0) * 60 * 24;
    }

    // Default: anggap sudah lama (>24 jam)
    99999
}

/// Extract URL base dari chapter URL (strip "-chapter-{number}").
///
/// "https://komikindo.ch/blue-lock-chapter-342/"
/// → "https://komikindo.ch/blue-lock"
pub fn extract_url_base(chapter_url: &str) -> String {
    static RE_STRIP_CH_SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-chapter-[\d.]+/?$").unwrap());
    let trimmed = chapter_url.trim_end_matches('/');
    RE_STRIP_CH_SUFFIX.replace(trimmed, "").to_string()
}

/// Construct chapter URL dari base dan nomor chapter.
///
/// ("https://komikindo.ch/blue-lock", 341)
/// → "https://komikindo.ch/blue-lock-chapter-341/"
#[allow(dead_code)]
pub fn construct_chapter_url(url_base: &str, chapter_number: f64) -> String {
    if chapter_number.fract() == 0.0 {
        format!("{}-chapter-{}/", url_base, chapter_number as i64)
    } else {
        // Fractal chapter: 124.2 → "chapter-124-2"
        let int_part = chapter_number.trunc() as i64;
        let frac_part = chapter_number.fract();
        // 0.2 → "2", 0.15 → "15", 0.10 → "10"
        let frac_formatted = format!("{:.10}", frac_part);
        let frac_str = frac_formatted
            .split('.')
            .nth(1)
            .unwrap_or("0")
            .trim_end_matches('0');
        format!("{}-chapter-{}-{}/", url_base, int_part, frac_str)
    }
}

/// Normalize URL: add BASE_URL prefix if relative.
fn normalize_url_full(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("{BASE_URL}{raw}")
    } else {
        format!("{BASE_URL}/{raw}")
    }
}

// ============================================================
// HELPERS
// ============================================================

#[allow(dead_code)]
fn find_common_prefix<'a>(paths: &'a [&'a str]) -> &'a str {
    if paths.is_empty() {
        return "";
    }
    if paths.len() == 1 {
        if let Some(pos) = paths[0].rfind('/') {
            return &paths[0][..pos];
        }
        return "";
    }

    let first = paths[0];
    let mut end = first.len();

    for path in &paths[1..] {
        let mut i = 0;
        let bytes1 = first.as_bytes();
        let bytes2 = path.as_bytes();
        while i < end && i < path.len() && bytes1[i] == bytes2[i] {
            i += 1;
        }
        end = i;
    }

    &first[..end]
}
