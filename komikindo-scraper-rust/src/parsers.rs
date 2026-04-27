/// HTML parsing functions.
/// CPU-bound parsing menggunakan `scraper` crate.
///
/// PERBAIKAN SLUG: Chapter URL diambil langsung dari hasil parse detail page,
/// BUKAN di-construct manual dari slug + chapter_number.

use crate::config::*;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::collections::{HashMap, HashSet};

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
    let re = Regex::new(r"-\d+x\d+\.").unwrap();
    re.replace(s, ".").to_string()
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

    // Coba selector utama
    let selector = Selector::parse("a.tip").unwrap();
    let mut links: Vec<ElementRef> = document.select(&selector).collect();

    // Fallback
    if links.is_empty() {
        let sel2 = Selector::parse("a[href*='/komik/']").unwrap();
        links = document.select(&sel2).collect();
    }

    let re = Regex::new(r"/komik/([^/]+)/?").unwrap();

    for link in &links {
        if let Some(href) = link.value().attr("href") {
            if href.contains("/komik/") {
                if let Some(caps) = re.captures(href) {
                    let slug = caps[1].to_string();
                    if !slug.is_empty() && seen.insert(slug.clone()) {
                        slugs.push(slug);
                    }
                }
            }
        }
    }

    slugs
}

/// Parse komik detail page HTML -> detail dict.
///
/// PERBAIKAN: chapter URL diambil langsung dari `<a href="...">` di detail page,
/// sehingga tidak perlu construct manual dari slug.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub fn set_image_data(&mut self, data: ChapterImageData) {
        self.cdn_domain_id = Some(data.cdn_domain_id);
        self.cdn_path_prefix = Some(data.cdn_path_prefix);
        self.image_filenames = Some(data.image_filenames);
        self.image_ext_ids = Some(data.image_ext_ids);
        self.total_images = Some(data.total_images as i32);
    }
}

/// Optimized image data dari chapter read page.
#[derive(Debug, Clone)]
pub struct ChapterImageData {
    pub cdn_domain_id: i16,
    pub cdn_path_prefix: String,
    pub image_filenames: Vec<String>,
    pub image_ext_ids: Vec<i16>,
    pub total_images: usize,
}

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

pub fn parse_homepage_updates(html: &str) -> Vec<HomepageUpdate> {
    let document = Html::parse_document(html);
    let mut updates = Vec::new();
    let mut seen = HashSet::new();

    let slug_re = Regex::new(r"/komik/([^/]+)/?").unwrap();
    let ch_re = Regex::new(r"(?i)chapter-([\d.]+)").unwrap();

    // Primary: div.listupd div.animepost
    let post_sel = Selector::parse("div.listupd div.animepost").unwrap();
    let posts: Vec<_> = document.select(&post_sel).collect();

    let items: Vec<ElementRef> = if !posts.is_empty() {
        posts
    } else {
        // Fallback: div.listupd div.bsx
        let bsx_sel = Selector::parse("div.listupd div.bsx, div.listupd div.bs div.bsx").unwrap();
        document.select(&bsx_sel).collect()
    };

    for post in &items {
        let mut item = HomepageUpdate {
            slug: String::new(),
            judul: None,
            latest_chapter_number: None,
            tipe: None,
            thumbnail: None,
        };

        // Title link
        let title_sel =
            Selector::parse("div.bigors div.tt h3 a, h3 a, a[href*='/komik/']").unwrap();
        if let Some(title_link) = post.select(&title_sel).next() {
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
                if let Some(caps) = slug_re.captures(&full_href) {
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
        let ch_sel = Selector::parse("div.bigors div.adds a[href*='chapter'], a[href*='-chapter-']")
            .unwrap();
        if let Some(ch_link) = post.select(&ch_sel).next() {
            if let Some(href) = ch_link.value().attr("href") {
                let full_href = if href.starts_with('/') {
                    format!("{BASE_URL}{href}")
                } else {
                    href.to_string()
                };
                if let Some(caps) = ch_re.captures(&full_href) {
                    if let Ok(num) = caps[1].parse::<f64>() {
                        item.latest_chapter_number = Some(num);
                    }
                }
            }
        }

        // Type
        if let Ok(type_sel) = Selector::parse("span.typeflag") {
            if let Some(type_el) = post.select(&type_sel).next() {
                for cls in type_el.value().classes() {
                    if cls != "typeflag" {
                        item.tipe = Some(cls.to_string());
                        break;
                    }
                }
            }
        }

        // Thumbnail
        let img_sel = Selector::parse("div.limit img, img").unwrap();
        if let Some(img) = post.select(&img_sel).next() {
            item.thumbnail = img.value().attr("src").map(|s| s.to_string());
        }

        updates.push(item);
    }

    updates
}

// ============================================================
// HELPERS
// ============================================================

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
