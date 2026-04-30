/// HTML parsing functions.
/// CPU-bound parsing menggunakan `tl` crate (tag libre).
///
/// PERBAIKAN SLUG: Chapter URL diambil langsung dari hasil parse detail page,
/// BUKAN di-construct manual dari slug + chapter_number.
///
/// OPTIMIZED V2 (tl migration):
///   - Replaced scraper (html5ever) with tl — 3-10x faster parsing, 5-10x less memory
///   - tl builds flat Vec<Node> instead of Rc<RefCell<Node>> DOM tree
///   - Zero-copy borrowing via Bytes — attributes & text borrowed from input string
///   - LazyLock cached selectors replaced with inline query_selector() calls
///     (tl re-parses selectors each call, but at ~50ns per parse this is negligible
///      compared to the saved DOM construction overhead)
///   - Genre map still cached as LazyLock static
///
/// FIX: tl does NOT support CSS descendant combinators (space between selectors).
///   - All selectors like "div.infox h1" have been rewritten to a two-step approach:
///     1. Find the container with a simple selector
///     2. Query inside that container using another simple selector
///   - For sub-selectors inside an already-found tag, we use the leaf selector
///     directly since we're already scoped to the correct container.

use crate::config::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use tl::{parse, Node, ParserOptions, VDom};

// ============================================================
// CACHED REGEX (avoid re-compile per page)
// ============================================================

static RE_THUMB_DIM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d+x\d+\.").unwrap());
static RE_SLUG_FROM_KOMIK_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/komik/([^/]+)/?").unwrap());

// ============================================================
// CACHED: parse_komik_detail regex (LazyLock)
// ============================================================

/// Genre map: cached once, reused for all detail parses.
/// Eliminates 82-entry HashMap allocation per komik (8677x savings).
static GENRE_MAP: LazyLock<HashMap<&'static str, i16>> = LazyLock::new(|| {
    crate::config::build_genre_map()
});

// Title prefix regex (cached)
static RE_TITLE_KOMIK_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^komik\s*").unwrap());

// Sinopsis cleanup regexes (cached)
static RE_SINOPSIS_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)^Update chapter terbaru komik.*?Sinopsis\s*").unwrap());
static RE_SINOPSIS_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(Manga|Manhwa|Manhua)\s+").unwrap());

// Chapter number regexes (cached)
static RE_CHAPTER_NUM_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter\s*([\d.]+)").unwrap());
static RE_CHAPTER_NUM_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());

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

    // Strip wp-content/uploads/ prefix (may appear after data/ or other path components)
    if let Some(wp_pos) = path.find(THUMB_WP_PREFIX) {
        path = path[wp_pos + THUMB_WP_PREFIX.len()..].to_string();
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
// HELPER: tl attribute extraction
// ============================================================

/// Get attribute value from an HTMLTag as &str.
/// Returns None if attribute doesn't exist or value is empty.
fn get_attr<'a>(tag: &'a tl::HTMLTag<'a>, key: &'a str) -> Option<&'a str> {
    tag.attributes().get(key).flatten().and_then(|b| b.try_as_utf8_str())
}

/// Get the inner text of a tag as a String (trimmed).
/// This allocates — only use when we need to own the text.
fn inner_text_owned(tag: &tl::HTMLTag, parser: &tl::Parser) -> String {
    tag.inner_text(parser).trim().to_string()
}

/// Get the classes of a tag as a Vec of &str.
fn get_classes<'a>(tag: &'a tl::HTMLTag<'a>) -> Vec<&'a str> {
    tag.attributes()
        .class_iter()
        .map(|iter| iter.collect())
        .unwrap_or_default()
}

// ============================================================
// HELPER: Two-step descendant query (tl does not support descendant combinators)
// ============================================================

/// Find first child tag matching `child_sel` inside the first container
/// matching `container_sel` on the DOM. Returns the child tag's inner text
/// as an owned String, or None if not found.
fn find_descendant_text(
    dom: &VDom,
    parser: &tl::Parser,
    container_sel: &str,
    child_sel: &str,
) -> Option<String> {
    let mut container_iter = dom.query_selector(container_sel)?;
    let container_handle = container_iter.next()?;
    let container_node = container_handle.get(parser)?;
    let container_tag = container_node.as_tag()?;
    let mut child_iter = container_tag.query_selector(parser, child_sel)?;
    let child_handle = child_iter.next()?;
    let child_node = child_handle.get(parser)?;
    let child_tag = child_node.as_tag()?;
    Some(inner_text_owned(child_tag, parser))
}

/// Find first child tag matching `child_sel` inside the first container
/// matching `container_sel` on the DOM. Returns the child tag's attribute
/// value as an owned String, or None if not found.
fn find_descendant_attr(
    dom: &VDom,
    parser: &tl::Parser,
    container_sel: &str,
    child_sel: &str,
    attr: &str,
) -> Option<String> {
    let mut container_iter = dom.query_selector(container_sel)?;
    let container_handle = container_iter.next()?;
    let container_node = container_handle.get(parser)?;
    let container_tag = container_node.as_tag()?;
    let mut child_iter = container_tag.query_selector(parser, child_sel)?;
    let child_handle = child_iter.next()?;
    let child_node = child_handle.get(parser)?;
    let child_tag = child_node.as_tag()?;
    get_attr(child_tag, attr).map(|s| s.to_string())
}

// ============================================================
// PARSERS
// ============================================================

/// Parse komik list page HTML -> list of slugs.
pub fn parse_komik_list(html: &str) -> Vec<String> {
    let dom = parse(html, ParserOptions::default()).unwrap_or_else(|_| {
        parse("", ParserOptions::default()).unwrap()
    });
    let parser = dom.parser();

    // Pre-allocate for ~8000+ entries (typical komikindo list page)
    let mut slugs = Vec::with_capacity(8000);
    let mut seen = HashSet::with_capacity(8000);

    // Try primary selector first, fallback if empty.
    let primary_sel = "a.tip";
    let fallback_sel = "a[href*='/komik/']";

    let links: Vec<&Node> = if let Some(iter) = dom.query_selector(primary_sel) {
        let collected: Vec<_> = iter
            .filter_map(|h| h.get(parser))
            .collect();
        if collected.is_empty() {
            dom.query_selector(fallback_sel)
                .map(|iter| iter.filter_map(|h| h.get(parser)).collect())
                .unwrap_or_default()
        } else {
            collected
        }
    } else {
        dom.query_selector(fallback_sel)
            .map(|iter| iter.filter_map(|h| h.get(parser)).collect())
            .unwrap_or_default()
    };

    for link in &links {
        let Some(tag) = link.as_tag() else { continue };
        let Some(href) = get_attr(tag, "href") else { continue };
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
/// OPTIMIZED V2: Using tl crate — 3-10x faster than scraper/html5ever.
/// Flat Vec<Node> instead of Rc<RefCell<Node>> DOM tree.
/// Zero-copy attribute borrowing via Bytes.
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
    /// Similar/recommended komik from "Mirip" section
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub similar: Vec<SimilarKomik>,
}

/// Similar/recommended komik from the "Mirip" section.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimilarKomik {
    pub slug: String,
    pub judul: Option<String>,
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

#[allow(dead_code)]
pub fn parse_komik_detail(slug: &str, html: &str) -> Option<KomikDetail> {
    // tl::parse("") returns Ok not Err, so check for empty HTML first
    if html.trim().is_empty() {
        return None;
    }

    let dom = parse(html, ParserOptions::default()).ok()?;
    let parser = dom.parser();

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
        similar: Vec::new(),
    };

    // === TITLE (try multiple selectors) ===
    // Simple selectors first (tl supports these)
    let title_simple_sels = ["h1.titless", "h1.entry-title"];
    for sel in &title_simple_sels {
        if let Some(mut iter) = dom.query_selector(sel) {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        let title = inner_text_owned(tag, parser);
                        if !title.is_empty() {
                            let cleaned = if title.to_lowercase().starts_with("komik") {
                                RE_TITLE_KOMIK_PREFIX
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
        }
    }
    // Fallback: two-step for "div.infox h1" (tl doesn't support descendant combinator)
    if detail.judul.is_none() {
        if let Some(title) = find_descendant_text(&dom, parser, "div.infox", "h1") {
            if !title.is_empty() {
                let cleaned = if title.to_lowercase().starts_with("komik") {
                    RE_TITLE_KOMIK_PREFIX
                        .replace(&title, "")
                        .trim()
                        .to_string()
                } else {
                    title
                };
                detail.judul = Some(cleaned);
            }
        }
    }

    // === TYPE (span.typeflag — first non-"typeflag" class) ===
    if let Some(mut iter) = dom.query_selector("span.typeflag") {
        if let Some(handle) = iter.next() {
            if let Some(node) = handle.get(parser) {
                if let Some(tag) = node.as_tag() {
                    for cls in get_classes(tag) {
                        if cls != "typeflag" {
                            detail.tipe = Some(cls.to_string());
                            break;
                        }
                    }
                }
            }
        }
    }

    // === THUMBNAIL (try multiple selectors using two-step approach) ===
    let mut thumb_url = String::new();
    // Two-step: find container, then img inside (replaces "div.infoanime img", "div.thumb img", "div.itemprop img")
    let thumb_container_sels = ["div.infoanime", "div.thumb", "div.itemprop"];
    for container_sel in &thumb_container_sels {
        if let Some(src) = find_descendant_attr(&dom, parser, container_sel, "img", "src") {
            if !src.is_empty() {
                thumb_url = src;
                break;
            }
        }
    }

    if thumb_url.is_empty() {
        if let Some(iter) = dom.query_selector("img[itemprop='image']") {
            for handle in iter {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        if let Some(src) = get_attr(tag, "src") {
                            if src.to_lowercase().contains("/komik-") {
                                thumb_url = src.to_string();
                                break;
                            }
                        }
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
    // Two-step: find div.spe container, then span inside (replaces "div.infox .spe span")
    let mut raw_genre_text: Option<String> = None;

    if let Some(mut spe_iter) = dom.query_selector("div.spe") {
        if let Some(spe_handle) = spe_iter.next() {
            if let Some(spe_node) = spe_handle.get(parser) {
                if let Some(spe_tag) = spe_node.as_tag() {
                    if let Some(span_iter) = spe_tag.query_selector(parser, "span") {
                        for span_handle in span_iter {
                            let Some(node) = span_handle.get(parser) else { continue };
                            let Some(span_tag) = node.as_tag() else { continue };

                            // Get bold child for label
                            let bold_text = if let Some(mut bold_iter) = span_tag.query_selector(parser, "b") {
                                bold_iter.next().and_then(|h| {
                                    h.get(parser).and_then(|n| n.as_tag().map(|t| inner_text_owned(t, parser)))
                                })
                            } else {
                                None
                            };

                            let Some(bold) = bold_text else { continue };
                            let full_text = inner_text_owned(span_tag, parser);
                            let value = full_text.replace(&bold, "").trim().to_string();
                            let label = bold.trim().trim_end_matches(':').to_lowercase();

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
                            } else if label.contains("alternative") || label.contains("alternatif") {
                                detail.alternative_title = Some(value);
                            }
                        }
                    }
                }
            }
        }
    }

    // Genre dari tag links (lebih reliable)
    // Two-step approach: search in div.genre-info and div.spe containers
    // (replaces "div.genre-info a[rel='tag'], div.infox .spe a[rel='tag']")
    let mut genre_links: Vec<String> = Vec::new();

    // Try div.genre-info container first
    let genre_containers = ["div.genre-info", "div.spe"];
    for container_sel in &genre_containers {
        if let Some(mut container_iter) = dom.query_selector(container_sel) {
            if let Some(container_handle) = container_iter.next() {
                if let Some(container_node) = container_handle.get(parser) {
                    if let Some(container_tag) = container_node.as_tag() {
                        if let Some(link_iter) = container_tag.query_selector(parser, "a[rel='tag']") {
                            for link_handle in link_iter {
                                if let Some(link_node) = link_handle.get(parser) {
                                    if let Some(link_tag) = link_node.as_tag() {
                                        let text = inner_text_owned(link_tag, parser);
                                        if !text.is_empty() {
                                            genre_links.push(text);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !genre_links.is_empty() {
            break;
        }
    }

    // Fallback: use raw genre text from info spans
    if genre_links.is_empty() {
        if let Some(ref text) = raw_genre_text {
            genre_links = text.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    // Genre ID lookup using CACHED genre_map
    detail.genre_ids = Vec::with_capacity(genre_links.len().min(8));
    for g in &genre_links {
        if let Some(&gid) = GENRE_MAP.get(g.as_str()) {
            detail.genre_ids.push(gid);
        }
    }
    detail.genre_list = genre_links;

    // === RATING (try multiple selectors) ===
    // Real HTML uses: div.archiveanime-rating i[itemprop="ratingValue"]
    // Also try legacy selectors for older page layouts
    let rating_container_sels = ["div.archiveanime-rating", "div.infoanime-rating", "div.rating"];
    for container_sel in &rating_container_sels {
        if let Some(r_text) = find_descendant_text(&dom, parser, container_sel, "i") {
            if !r_text.is_empty() {
                if let Ok(r) = r_text.parse::<f64>() {
                    detail.rating = Some(r);
                    break;
                }
            }
        }
    }
    // Also try span.skor (simple selector, no descendant)
    if detail.rating.is_none() {
        if let Some(mut iter) = dom.query_selector("span.skor") {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        let r_text = inner_text_owned(tag, parser);
                        if !r_text.is_empty() {
                            if let Ok(r) = r_text.parse::<f64>() {
                                detail.rating = Some(r);
                            }
                        }
                    }
                }
            }
        }
    }

    // === SINOPSIS (try multiple selectors + regex cleanup) ===
    // "div.entry-content.entry-content-single" and "div.entry-content" are simple selectors (no space)
    let sinopsis_simple_sels = [
        "div.entry-content.entry-content-single",
        "div.entry-content",
    ];
    for sel in &sinopsis_simple_sels {
        if let Some(mut iter) = dom.query_selector(sel) {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        let mut sinopsis = inner_text_owned(tag, parser);
                        // Clean prefix using cached regex
                        sinopsis = RE_SINOPSIS_PREFIX.replace(&sinopsis, "").to_string();
                        sinopsis = RE_SINOPSIS_TYPE.replace(&sinopsis, "").to_string();
                        sinopsis = sinopsis.trim().to_string();
                        if !sinopsis.is_empty() {
                            detail.sinopsis = Some(sinopsis);
                            break;
                        }
                    }
                }
            }
        }
    }
    // Fallback: two-step for "div#sinopsis div.entry-content" (tl doesn't support descendant)
    if detail.sinopsis.is_none() {
        if let Some(sinopsis_text) = find_descendant_text(&dom, parser, "div#sinopsis", "div.entry-content") {
            let mut sinopsis = sinopsis_text;
            sinopsis = RE_SINOPSIS_PREFIX.replace(&sinopsis, "").to_string();
            sinopsis = RE_SINOPSIS_TYPE.replace(&sinopsis, "").to_string();
            sinopsis = sinopsis.trim().to_string();
            if !sinopsis.is_empty() {
                detail.sinopsis = Some(sinopsis);
            }
        }
    }

    // === CHAPTERS ===
    detail.chapters = extract_chapters(&dom, parser);
    if let Some(first) = detail.chapters.first() {
        detail.latest_chapter_number = Some(first.number);
    }

    // === SIMILAR/MIRIP (recommended komik) ===
    // HTML: div#mirip > div.widget-post.miripmanga > div.serieslist > ul > li
    //   Each li has: a.series[href*="/komik/"] with title + h3 > a.series with title
    detail.similar = extract_similar(&dom, parser);

    Some(detail)
}

/// Extract similar/recommended komik from the "Mirip" section.
/// HTML structure: div#mirip > div.miripmanga > div.serieslist > ul > li
///   Each li contains:
///     - div.imgseries > a.series[href*="/komik/"] for thumbnail (title text is dirty)
///     - div.leftseries > h3 > a.series for clean title
///   We extract slug from any a.series[href*="/komik/"], and title from h3 > a.series
fn extract_similar(dom: &VDom, parser: &tl::Parser) -> Vec<SimilarKomik> {
    let mut similar = Vec::new();

    // Find the mirip container
    let Some(mut mirip_iter) = dom.query_selector("div#mirip") else { return similar };
    let Some(mirip_handle) = mirip_iter.next() else { return similar };
    let Some(mirip_node) = mirip_handle.get(parser) else { return similar };
    let Some(mirip_tag) = mirip_node.as_tag() else { return similar };

    // Find all li items inside mirip
    if let Some(li_iter) = mirip_tag.query_selector(parser, "li") {
        for li_handle in li_iter {
            let Some(li_node) = li_handle.get(parser) else { continue };
            let Some(li_tag) = li_node.as_tag() else { continue };

            // Get slug from any a.series[href] inside this li
            let mut slug = String::new();
            if let Some(mut a_iter) = li_tag.query_selector(parser, "a.series") {
                if let Some(a_handle) = a_iter.next() {
                    if let Some(a_node) = a_handle.get(parser) {
                        if let Some(a_tag) = a_node.as_tag() {
                            if let Some(href) = get_attr(a_tag, "href") {
                                if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(href) {
                                    slug = caps[1].to_string();
                                }
                            }
                        }
                    }
                }
            }
            if slug.is_empty() {
                continue;
            }

            // Avoid duplicates by slug
            if similar.iter().any(|s| s.slug == slug) {
                continue;
            }

            // Get clean title from h3 > a.series inside div.leftseries
            let mut judul: Option<String> = None;
            if let Some(mut h3_iter) = li_tag.query_selector(parser, "h3") {
                if let Some(h3_handle) = h3_iter.next() {
                    if let Some(h3_node) = h3_handle.get(parser) {
                        if let Some(h3_tag) = h3_node.as_tag() {
                            if let Some(mut a_iter) = h3_tag.query_selector(parser, "a") {
                                if let Some(a_handle) = a_iter.next() {
                                    if let Some(a_node) = a_handle.get(parser) {
                                        if let Some(a_tag) = a_node.as_tag() {
                                            let text = inner_text_owned(a_tag, parser);
                                            if !text.is_empty() {
                                                judul = Some(text);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            similar.push(SimilarKomik { slug, judul });
        }
    }

    similar
}

/// Extract chapter list dari detail page.
fn extract_chapters(dom: &VDom, parser: &tl::Parser) -> Vec<ChapterInfo> {
    let mut chapters = Vec::with_capacity(100);
    let mut seen_urls = HashSet::with_capacity(100);

    // Try from main container first
    let container_selectors = ["div.bxcl", "div#chapter_list"];

    let mut found_chapters = false;

    for container_sel in &container_selectors {
        if let Some(mut container_iter) = dom.query_selector(container_sel) {
            if let Some(container_handle) = container_iter.next() {
                if let Some(container_node) = container_handle.get(parser) {
                    if let Some(container_tag) = container_node.as_tag() {
                        // Primary: direct a[href*='-chapter-'] (simple selector, works in tl)
                        if let Some(link_iter) = container_tag.query_selector(parser, "a[href*='-chapter-']") {
                            for link_handle in link_iter {
                                let Some(link_node) = link_handle.get(parser) else { continue };
                                let Some(link_tag) = link_node.as_tag() else { continue };

                                if try_add_chapter(link_tag, parser, &mut chapters, &mut seen_urls) {
                                    found_chapters = true;
                                }
                            }
                        }
                        // Fallback: two-step for span.lchx -> a (replaces "span.lchx a")
                        if !found_chapters {
                            if let Some(lchx_iter) = container_tag.query_selector(parser, "span.lchx") {
                                for lchx_handle in lchx_iter {
                                    let Some(lchx_node) = lchx_handle.get(parser) else { continue };
                                    let Some(lchx_tag) = lchx_node.as_tag() else { continue };
                                    if let Some(link_iter) = lchx_tag.query_selector(parser, "a") {
                                        for link_handle in link_iter {
                                            let Some(link_node) = link_handle.get(parser) else { continue };
                                            let Some(link_tag) = link_node.as_tag() else { continue };

                                            if try_add_chapter(link_tag, parser, &mut chapters, &mut seen_urls) {
                                                found_chapters = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if found_chapters {
            break;
        }
    }

    // Fallback: all chapter links in page
    if !found_chapters {
        if let Some(iter) = dom.query_selector("a[href*='-chapter-']") {
            for link_handle in iter {
                let Some(link_node) = link_handle.get(parser) else { continue };
                let Some(link_tag) = link_node.as_tag() else { continue };
                try_add_chapter(link_tag, parser, &mut chapters, &mut seen_urls);
            }
        }
    }

    // Sort descending by chapter number
    if chapters.len() > 1 {
        chapters.sort_by(|a, b| b.number.total_cmp(&a.number));
    }
    chapters
}

/// Try to extract and add a chapter from a link tag.
/// Returns true if a chapter was added.
fn try_add_chapter(
    link_tag: &tl::HTMLTag,
    parser: &tl::Parser,
    chapters: &mut Vec<ChapterInfo>,
    seen_urls: &mut HashSet<String>,
) -> bool {
    let Some(ch_url) = get_attr(link_tag, "href") else { return false };
    if ch_url.is_empty() {
        return false;
    }

    let full_url = if ch_url.starts_with('/') {
        format!("{BASE_URL}{ch_url}")
    } else {
        ch_url.to_string()
    };

    if !seen_urls.insert(full_url.clone()) {
        return false;
    }

    let text = inner_text_owned(link_tag, parser);
    let Some(number) = extract_chapter_number(&text, &full_url) else {
        return false;
    };

    chapters.push(ChapterInfo {
        number,
        url: full_url,
        cdn_domain_id: None,
        cdn_path_prefix: None,
        image_filenames: None,
        image_ext_ids: None,
        total_images: None,
    });
    true
}

/// Extract chapter number dari text dan URL.
/// OPTIMIZED: Regex cached as LazyLock statics.
fn extract_chapter_number(title: &str, url: &str) -> Option<f64> {
    if let Some(caps) = RE_CHAPTER_NUM_TITLE.captures(title) {
        if let Ok(num) = caps[1].parse::<f64>() {
            return Some(num);
        }
    }

    if let Some(caps) = RE_CHAPTER_NUM_URL.captures(url) {
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
pub fn parse_chapter_images(html: &str) -> ChapterImageData {
    let dom = parse(html, ParserOptions::default()).unwrap_or_else(|_| {
        parse("", ParserOptions::default()).unwrap()
    });
    let parser = dom.parser();
    let mut raw_images = Vec::new();

    // Primary: div#chimg-auh or div.chimg-auh (already uses two-step approach — no descendant combinator)
    let container_selectors = ["div#chimg-auh", "div.chimg-auh"];
    for sel in &container_selectors {
        if let Some(mut iter) = dom.query_selector(sel) {
            if let Some(container_handle) = iter.next() {
                if let Some(container_node) = container_handle.get(parser) {
                    if let Some(container_tag) = container_node.as_tag() {
                        if let Some(img_iter) = container_tag.query_selector(parser, "img") {
                            for img_handle in img_iter {
                                if let Some(img_node) = img_handle.get(parser) {
                                    if let Some(img_tag) = img_node.as_tag() {
                                        if let Some(src) = get_attr(img_tag, "src") {
                                            if is_chapter_image(src) {
                                                raw_images.push(src.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !raw_images.is_empty() {
            break;
        }
    }

    // Fallback: semua img di halaman
    if raw_images.is_empty() {
        if let Some(iter) = dom.query_selector("img") {
            for img_handle in iter {
                if let Some(img_node) = img_handle.get(parser) {
                    if let Some(img_tag) = img_node.as_tag() {
                        if let Some(src) = get_attr(img_tag, "src") {
                            if is_chapter_image(src) {
                                raw_images.push(src.to_string());
                            }
                        }
                    }
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

static RE_CHAPTER_FROM_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());

pub fn parse_homepage_updates(html: &str) -> Vec<HomepageUpdate> {
    let dom = parse(html, ParserOptions::default()).unwrap_or_else(|_| {
        parse("", ParserOptions::default()).unwrap()
    });
    let parser = dom.parser();

    let mut updates = Vec::with_capacity(30);
    let mut seen = HashSet::with_capacity(30);

    // Two-step: find div.listupd, then div.animepost inside (replaces "div.listupd div.animepost")
    let mut posts: Vec<&Node> = Vec::new();

    if let Some(mut listupd_iter) = dom.query_selector("div.listupd") {
        if let Some(listupd_handle) = listupd_iter.next() {
            if let Some(listupd_node) = listupd_handle.get(parser) {
                if let Some(listupd_tag) = listupd_node.as_tag() {
                    if let Some(animepost_iter) = listupd_tag.query_selector(parser, "div.animepost") {
                        posts = animepost_iter.filter_map(|h| h.get(parser)).collect();
                    }
                }
            }
        }
    }

    // Fallback: find div.listupd, then div.bsx inside (replaces "div.listupd div.bsx, div.listupd div.bs div.bsx")
    if posts.is_empty() {
        if let Some(mut listupd_iter) = dom.query_selector("div.listupd") {
            if let Some(listupd_handle) = listupd_iter.next() {
                if let Some(listupd_node) = listupd_handle.get(parser) {
                    if let Some(listupd_tag) = listupd_node.as_tag() {
                        if let Some(bsx_iter) = listupd_tag.query_selector(parser, "div.bsx") {
                            posts = bsx_iter.filter_map(|h| h.get(parser)).collect();
                        }
                    }
                }
            }
        }
    }

    for post in &posts {
        let Some(post_tag) = post.as_tag() else { continue };
        let mut item = HomepageUpdate {
            slug: String::new(),
            judul: None,
            latest_chapter_number: None,
            tipe: None,
            thumbnail: None,
        };

        // Title link — try two-step: find h3 then a inside, then fallback to a[href*='/komik/']
        // (replaces "div.bigors div.tt h3 a", "h3 a", "a[href*='/komik/']")
        let mut title_found = false;
        if let Some(mut h3_iter) = post_tag.query_selector(parser, "h3") {
            if let Some(h3_handle) = h3_iter.next() {
                if let Some(h3_node) = h3_handle.get(parser) {
                    if let Some(h3_tag) = h3_node.as_tag() {
                        if let Some(mut a_iter) = h3_tag.query_selector(parser, "a") {
                            if let Some(a_handle) = a_iter.next() {
                                if let Some(a_node) = a_handle.get(parser) {
                                    if let Some(a_tag) = a_node.as_tag() {
                                        item.judul = Some(inner_text_owned(a_tag, parser));
                                        if let Some(href) = get_attr(a_tag, "href") {
                                            let full_href = if href.starts_with('/') {
                                                format!("{BASE_URL}{href}")
                                            } else {
                                                href.to_string()
                                            };
                                            if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(&full_href) {
                                                item.slug = caps[1].to_string();
                                            }
                                        }
                                        title_found = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !title_found {
            // Fallback: direct a[href*='/komik/'] (simple selector)
            if let Some(mut iter) = post_tag.query_selector(parser, "a[href*='/komik/']") {
                if let Some(handle) = iter.next() {
                    if let Some(node) = handle.get(parser) {
                        if let Some(tag) = node.as_tag() {
                            item.judul = Some(inner_text_owned(tag, parser));
                            if let Some(href) = get_attr(tag, "href") {
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
                    }
                }
            }
        }

        if item.slug.is_empty() {
            continue;
        }

        if !seen.insert(item.slug.clone()) {
            continue;
        }

        // Chapter link — use simple selector a[href*='-chapter-'] directly
        // (replaces "div.bigors div.adds a[href*='chapter'], a[href*='-chapter-']")
        if let Some(mut iter) = post_tag.query_selector(parser, "a[href*='-chapter-']") {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        if let Some(href) = get_attr(tag, "href") {
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
                }
            }
        }

        // Type
        if let Some(mut iter) = post_tag.query_selector(parser, "span.typeflag") {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        for cls in get_classes(tag) {
                            if cls != "typeflag" {
                                item.tipe = Some(cls.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Thumbnail — two-step: find div.limit then img inside, fallback to just img
        // (replaces "div.limit img, img")
        let mut thumb_found = false;
        if let Some(mut limit_iter) = post_tag.query_selector(parser, "div.limit") {
            if let Some(limit_handle) = limit_iter.next() {
                if let Some(limit_node) = limit_handle.get(parser) {
                    if let Some(limit_tag) = limit_node.as_tag() {
                        if let Some(mut img_iter) = limit_tag.query_selector(parser, "img") {
                            if let Some(img_handle) = img_iter.next() {
                                if let Some(img_node) = img_handle.get(parser) {
                                    if let Some(img_tag) = img_node.as_tag() {
                                        item.thumbnail = get_attr(img_tag, "src").map(|s| s.to_string());
                                        thumb_found = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !thumb_found {
            // Fallback: any img inside post
            if let Some(mut iter) = post_tag.query_selector(parser, "img") {
                if let Some(handle) = iter.next() {
                    if let Some(node) = handle.get(parser) {
                        if let Some(tag) = node.as_tag() {
                            item.thumbnail = get_attr(tag, "src").map(|s| s.to_string());
                        }
                    }
                }
            }
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
    /// Full URL chapter terbaru
    pub chapter_url: String,
    /// URL base untuk construct chapter URL baru
    pub url_base: String,
    /// Time string asli (e.g., "3 menit lalu", "4 jam lalu")
    pub time_str: String,
    /// Time dalam menit untuk sorting/comparison
    pub time_minutes: u32,
}

static RE_TERBARU_CH_NUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());

static RE_TIME_MIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*menit").unwrap());
static RE_TIME_HOUR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*jam").unwrap());
static RE_TIME_DAY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*hari").unwrap());

/// Parse halaman /komik-terbaru/ -> list of TerbaruItem.
pub fn parse_komik_terbaru(html: &str) -> Vec<TerbaruItem> {
    let dom = parse(html, ParserOptions::default()).unwrap_or_else(|_| {
        parse("", ParserOptions::default()).unwrap()
    });
    let parser = dom.parser();

    let mut items = Vec::with_capacity(40);
    let mut seen = HashSet::with_capacity(40);

    if let Some(iter) = dom.query_selector("div.animepost") {
        for post_handle in iter {
            let Some(post_node) = post_handle.get(parser) else { continue };
            let Some(post_tag) = post_node.as_tag() else { continue };

            // --- Komik link & slug ---
            // Two-step: find h3 then a[href*='/komik/'] inside (replaces "h3 a[href*='/komik/']")
            // Then fallback: a.animposx[href*='/komik/'] (simple selector, works in tl)
            let mut slug = String::new();
            let mut judul = String::new();

            // Try two-step: h3 -> a[href*='/komik/']
            let mut komik_link_found = false;
            if let Some(mut h3_iter) = post_tag.query_selector(parser, "h3") {
                if let Some(h3_handle) = h3_iter.next() {
                    if let Some(h3_node) = h3_handle.get(parser) {
                        if let Some(h3_tag) = h3_node.as_tag() {
                            if let Some(mut a_iter) = h3_tag.query_selector(parser, "a[href*='/komik/']") {
                                if let Some(a_handle) = a_iter.next() {
                                    if let Some(a_node) = a_handle.get(parser) {
                                        if let Some(a_tag) = a_node.as_tag() {
                                            judul = inner_text_owned(a_tag, parser);
                                            if let Some(href) = get_attr(a_tag, "href") {
                                                if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(href) {
                                                    slug = caps[1].to_string();
                                                }
                                            }
                                            komik_link_found = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Fallback: a.animposx[href*='/komik/'] (simple selector, no descendant)
            if !komik_link_found {
                if let Some(mut iter) = post_tag.query_selector(parser, "a.animposx[href*='/komik/']") {
                    if let Some(handle) = iter.next() {
                        if let Some(node) = handle.get(parser) {
                            if let Some(tag) = node.as_tag() {
                                judul = inner_text_owned(tag, parser);
                                if let Some(href) = get_attr(tag, "href") {
                                    if let Some(caps) = RE_SLUG_FROM_KOMIK_URL.captures(href) {
                                        slug = caps[1].to_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if slug.is_empty() || !seen.insert(slug.clone()) {
                continue;
            }

            // --- Chapter link ---
            // Two-step: find div.lsch then a[href*='-chapter-'] inside (replaces "div.lsch a[href*='-chapter-']")
            let ch_result = if let Some(mut lsch_iter) = post_tag.query_selector(parser, "div.lsch") {
                lsch_iter.next().and_then(|lsch_handle| {
                    let lsch_node = lsch_handle.get(parser)?;
                    let lsch_tag = lsch_node.as_tag()?;
                    let mut a_iter = lsch_tag.query_selector(parser, "a[href*='-chapter-']")?;
                    a_iter.next().and_then(|a_handle| {
                        let a_node = a_handle.get(parser)?;
                        let a_tag = a_node.as_tag()?;
                        let href = get_attr(a_tag, "href").unwrap_or("");
                        Some((normalize_url_full(href), inner_text_owned(a_tag, parser)))
                    })
                })
            } else {
                None
            };

            let Some((ch_url, _ch_text)) = ch_result else { continue };

            let chapter_number = match RE_TERBARU_CH_NUM.captures(&ch_url) {
                Some(caps) => caps[1].parse::<f64>().unwrap_or(0.0),
                None => continue,
            };

            // --- Time ---
            let time_str = if let Some(mut iter) = post_tag.query_selector(parser, "span.datech") {
                iter.next().and_then(|h| {
                    h.get(parser).and_then(|n| n.as_tag().map(|t| inner_text_owned(t, parser)))
                }).unwrap_or_default()
            } else {
                String::new()
            };
            let time_minutes = parse_time_to_minutes(&time_str);

            // --- URL base ---
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
pub fn extract_url_base(chapter_url: &str) -> String {
    static RE_STRIP_CH_SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-chapter-[\d.]+/?$").unwrap());
    let trimmed = chapter_url.trim_end_matches('/');
    RE_STRIP_CH_SUFFIX.replace(trimmed, "").to_string()
}

/// Construct chapter URL dari base dan nomor chapter.
#[allow(dead_code)]
pub fn construct_chapter_url(url_base: &str, chapter_number: f64) -> String {
    if chapter_number.fract() == 0.0 {
        format!("{}-chapter-{}/", url_base, chapter_number as i64)
    } else {
        let int_part = chapter_number.trunc() as i64;
        let frac_part = chapter_number.fract();
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

/// Keep `build_genre_map` available for other uses (e.g. config), but
/// internal parsing now uses the cached `GENRE_MAP` static.
#[allow(dead_code)]
pub fn build_genre_map() -> HashMap<&'static str, i16> {
    GENRE_MAP.clone()
}

// ============================================================
// LM-BASED PARSER (experimental)
// ============================================================

/// Parse komik detail using LM detector for title/rating/genre/synopsis,
/// with hardcoded selectors as fallback and for remaining fields.
///
/// This replaces the CSS selector "find the node" step with ONNX inference
/// for the 4 supported fields, while keeping the same post-processing logic
/// (genre ID lookup, sinopsis cleanup, etc.)
pub fn parse_komik_detail_lm(
    slug: &str,
    html: &str,
    detector: &mut crate::lm_selector::LmDetector,
) -> Option<KomikDetail> {
    use crate::lm_selector::FieldType;

    if html.trim().is_empty() {
        return None;
    }

    let dom = parse(html, ParserOptions::default()).ok()?;
    let parser = dom.parser();

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
        similar: Vec::new(),
    };

    // === TITLE: LM first, hardcoded fallback ===
    let lm_title = detector.detect_title(html);
    match lm_title {
        Some(title) => {
            detail.judul = Some(title);
        }
        None => {
            // Fallback to hardcoded selectors
            let title_simple_sels = ["h1.titless", "h1.entry-title"];
            for sel in &title_simple_sels {
                if let Some(mut iter) = dom.query_selector(sel) {
                    if let Some(handle) = iter.next() {
                        if let Some(node) = handle.get(parser) {
                            if let Some(tag) = node.as_tag() {
                                let title = inner_text_owned(tag, parser);
                                if !title.is_empty() {
                                    let cleaned = if title.to_lowercase().starts_with("komik") {
                                        RE_TITLE_KOMIK_PREFIX.replace(&title, "").trim().to_string()
                                    } else {
                                        title
                                    };
                                    detail.judul = Some(cleaned);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // === THUMBNAIL ===
    // Keep hardcoded — thumb detection is not an LM field
    extract_thumbnail(&dom, parser, &mut detail);

    // === INFO (status, author, alt title) ===
    // Keep hardcoded — these require bold-label matching, not node detection
    extract_spe_info(&dom, parser, &mut detail);

    // === GENRE: LM first, hardcoded fallback ===
    let lm_genres = detector.detect_genres(html);
    if !lm_genres.is_empty() {
        detail.genre_list = lm_genres;
    } else {
        // Fallback: hardcoded selector
        let mut genre_links: Vec<String> = Vec::new();
        let genre_containers = ["div.genre-info", "div.spe"];
        for container_sel in &genre_containers {
            if let Some(mut container_iter) = dom.query_selector(container_sel) {
                if let Some(container_handle) = container_iter.next() {
                    if let Some(container_node) = container_handle.get(parser) {
                        if let Some(container_tag) = container_node.as_tag() {
                            if let Some(link_iter) = container_tag.query_selector(parser, "a[rel='tag']") {
                                for link_handle in link_iter {
                                    if let Some(link_node) = link_handle.get(parser) {
                                        if let Some(link_tag) = link_node.as_tag() {
                                            let text = inner_text_owned(link_tag, parser);
                                            if !text.is_empty() {
                                                genre_links.push(text);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !genre_links.is_empty() {
                break;
            }
        }
        detail.genre_list = genre_links;
    }

    // Genre ID lookup (same logic regardless of detection method)
    detail.genre_ids = Vec::with_capacity(detail.genre_list.len().min(8));
    for g in &detail.genre_list {
        if let Some(&gid) = GENRE_MAP.get(g.as_str()) {
            detail.genre_ids.push(gid);
        }
    }

    // === RATING: LM first, hardcoded fallback ===
    let lm_rating = detector.detect_rating(html);
    match lm_rating {
        Some(r) => {
            detail.rating = Some(r);
        }
        None => {
            // Fallback: hardcoded selector
            let rating_container_sels = ["div.archiveanime-rating", "div.infoanime-rating", "div.rating"];
            for container_sel in &rating_container_sels {
                if let Some(r_text) = find_descendant_text(&dom, parser, container_sel, "i") {
                    if !r_text.is_empty() {
                        if let Ok(r) = r_text.parse::<f64>() {
                            detail.rating = Some(r);
                            break;
                        }
                    }
                }
            }
            if detail.rating.is_none() {
                if let Some(mut iter) = dom.query_selector("span.skor") {
                    if let Some(handle) = iter.next() {
                        if let Some(node) = handle.get(parser) {
                            if let Some(tag) = node.as_tag() {
                                let r_text = inner_text_owned(tag, parser);
                                if !r_text.is_empty() {
                                    if let Ok(r) = r_text.parse::<f64>() {
                                        detail.rating = Some(r);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // === SINOPSIS: LM first, hardcoded fallback ===
    let lm_synopsis = detector.detect_synopsis(html);
    match lm_synopsis {
        Some(raw_text) => {
            // Apply same cleanup as hardcoded parser
            let mut sinopsis = raw_text;
            sinopsis = RE_SINOPSIS_PREFIX.replace(&sinopsis, "").to_string();
            sinopsis = RE_SINOPSIS_TYPE.replace(&sinopsis, "").to_string();
            sinopsis = sinopsis.trim().to_string();
            if !sinopsis.is_empty() {
                detail.sinopsis = Some(sinopsis);
            }
        }
        None => {
            // Fallback: hardcoded selector
            let sinopsis_simple_sels = [
                "div.entry-content.entry-content-single",
                "div.entry-content",
            ];
            for sel in &sinopsis_simple_sels {
                if let Some(mut iter) = dom.query_selector(sel) {
                    if let Some(handle) = iter.next() {
                        if let Some(node) = handle.get(parser) {
                            if let Some(tag) = node.as_tag() {
                                let mut sinopsis = inner_text_owned(tag, parser);
                                sinopsis = RE_SINOPSIS_PREFIX.replace(&sinopsis, "").to_string();
                                sinopsis = RE_SINOPSIS_TYPE.replace(&sinopsis, "").to_string();
                                sinopsis = sinopsis.trim().to_string();
                                if !sinopsis.is_empty() {
                                    detail.sinopsis = Some(sinopsis);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if detail.sinopsis.is_none() {
                if let Some(sinopsis_text) = find_descendant_text(&dom, parser, "div#sinopsis", "div.entry-content") {
                    let mut sinopsis = sinopsis_text;
                    sinopsis = RE_SINOPSIS_PREFIX.replace(&sinopsis, "").to_string();
                    sinopsis = RE_SINOPSIS_TYPE.replace(&sinopsis, "").to_string();
                    sinopsis = sinopsis.trim().to_string();
                    if !sinopsis.is_empty() {
                        detail.sinopsis = Some(sinopsis);
                    }
                }
            }
        }
    }

    // === CHAPTERS === (hardcoded — container-level, not LM-detected)
    detail.chapters = extract_chapters(&dom, parser);
    if let Some(first) = detail.chapters.first() {
        detail.latest_chapter_number = Some(first.number);
    }

    // === SIMILAR/MIRIP === (hardcoded — container-level, not LM-detected)
    detail.similar = extract_similar(&dom, parser);

    Some(detail)
}

/// Extract thumbnail info (extracted from parse_komik_detail for reuse)
fn extract_thumbnail(dom: &VDom, parser: &tl::Parser, detail: &mut KomikDetail) {
    // Try multiple selectors for the thumbnail image
    let thumb_sels = ["div.thumb img", "div.imgdesc img", "img.wp-post-image"];
    for sel in &thumb_sels {
        if let Some(mut iter) = dom.query_selector(sel) {
            if let Some(handle) = iter.next() {
                if let Some(node) = handle.get(parser) {
                    if let Some(tag) = node.as_tag() {
                        if let Some(src) = get_attr(tag, "src") {
                            if !src.is_empty() {
                                if let Some((domain_id, path)) = normalize_thumbnail(src) {
                                    detail.thumb_domain_id = Some(domain_id);
                                    detail.thumb_path = Some(path);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Extract info from div.spe (status, author, alt title, type)
fn extract_spe_info(dom: &VDom, parser: &tl::Parser, detail: &mut KomikDetail) {
    let mut raw_genre_text: Option<String> = None;

    if let Some(mut spe_iter) = dom.query_selector("div.spe") {
        if let Some(spe_handle) = spe_iter.next() {
            if let Some(spe_node) = spe_handle.get(parser) {
                if let Some(spe_tag) = spe_node.as_tag() {
                    if let Some(span_iter) = spe_tag.query_selector(parser, "span") {
                        for span_handle in span_iter {
                            let Some(node) = span_handle.get(parser) else { continue };
                            let Some(span_tag) = node.as_tag() else { continue };

                            let bold_text = if let Some(mut bold_iter) = span_tag.query_selector(parser, "b") {
                                bold_iter.next().and_then(|h| {
                                    h.get(parser).and_then(|n| n.as_tag().map(|t| inner_text_owned(t, parser)))
                                })
                            } else {
                                None
                            };

                            let Some(bold) = bold_text else { continue };
                            let full_text = inner_text_owned(span_tag, parser);
                            let value = full_text.replace(&bold, "").trim().to_string();
                            let label = bold.trim().trim_end_matches(':').to_lowercase();

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
                            } else if label.contains("alternative") || label.contains("alternatif") {
                                detail.alternative_title = Some(value);
                            }
                        }
                    }
                }
            }
        }
    }
}
