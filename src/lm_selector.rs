/// LM-based Selector Engine — Unified Multi-Field Detection (1 AI)
///
/// Uses a single ONNX model to detect ALL fields simultaneously.
/// Instead of 9 separate models, one model outputs 9 probabilities:
///   [title_prob, rating_prob, genre_prob, synopsis_prob,
///    alt_title_prob, author_prob, status_prob, similar_prob, chapters_prob]
///
/// Architecture:
///   HTML → DOM Walk (1 pass) → Feature Vectors → ONNX (1 inference per node) → All Fields
///
/// Feature vector (44 dims):
///   [0-8]   Tag one-hot: h1, h2, h3, span, div, a, td, i, meta
///   [9-13]  Class contains: title, entry, info, rating, archive
///   [14]    Has non-empty id
///   [15-19] Structural: depth, sibling_index, sibling_count, child_count, text_length_log1p
///   [20]    Text starts with "Komik"
///   [21-22] Parent: is_div, has_info_class
///   [23-24] Attribute: has_itemprop, itemprop_value (ratingValue=1, else 0)
///   [25-27] Inside ancestor: spe, infox, infoanime
///   [28]    Bold text ratio
///   [29-30] Link count, has rel=tag
///   [31]    Font size indicator
///   [32-34] Bold text label patterns: contains_status, contains_author, contains_alternative
///   [35]    Class contains: desc/synopsis/entry-content
///   [36]    Has href containing "-chapter-"
///   [37]    Inside ancestor: mirip/bxcl
///   [38]    Has class: lchx/series
///   [39]    Text contains "Chapter"
///   [40]    Text matches rating float pattern (e.g. "7.5", "8.0")
///   [41]    Text matches chapter number pattern (e.g. "Chapter 1", "Bab 45")
///   [42]    Text contains a 4-digit year (1900-2099)
///   [43]    Normalized document position (0=top, 1=bottom)

use anyhow::Result;
use ort::session::Session;
use std::collections::HashSet;
use std::sync::OnceLock;

// Precompiled regex patterns for numeric features (compiled once, reused)
static RATING_FLOAT_RE: OnceLock<regex::Regex> = OnceLock::new();
static CHAPTER_NUMBER_RE: OnceLock<regex::Regex> = OnceLock::new();
static YEAR_RE: OnceLock<regex::Regex> = OnceLock::new();

fn rating_float_re() -> &'static regex::Regex {
    RATING_FLOAT_RE.get_or_init(|| regex::Regex::new(r"\b\d\.\d\b").unwrap())
}

fn chapter_number_re() -> &'static regex::Regex {
    CHAPTER_NUMBER_RE.get_or_init(|| regex::Regex::new(r"(?i)(chapter|bab)\s*\d+").unwrap())
}

fn year_re() -> &'static regex::Regex {
    YEAR_RE.get_or_init(|| regex::Regex::new(r"\b(19|20)\d{2}\b").unwrap())
}

/// Number of features per DOM node
pub const NUM_FEATURES: usize = 44;

/// Number of output fields
pub const NUM_FIELDS: usize = 9;

/// Tag vocabulary for one-hot encoding
const TAG_VOCAB: &[&str] = &["h1", "h2", "h3", "span", "div", "a", "td", "i", "meta"];

/// Tags to skip during tree walking
const SKIP_TAGS: &[&str] = &["script", "style", "noscript", "head"];

/// Tags that are candidates for ANY field detection
const CANDIDATE_TAGS: &[&str] = &[
    "h1", "h2", "h3", "span", "div", "a", "td", "i", "meta",
    "p", "b", "strong", "label", "section", "article",
];

/// Field names in output order — must match Python FIELD_NAMES
const FIELD_NAMES_RUST: &[&str] = &[
    "title", "rating", "genre", "synopsis", "alt_title", "author", "status", "similar", "chapters",
];

/// Which fields we can detect with LM — order must match Python FIELD_NAMES
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    Title,
    Rating,
    Genre,
    Synopsis,
    AltTitle,
    Author,
    Status,
    Similar,
    Chapters,
}

impl FieldType {
    /// Column index in the multi-label output (must match Python FIELD_NAMES order)
    pub fn output_index(&self) -> usize {
        match self {
            FieldType::Title => 0,
            FieldType::Rating => 1,
            FieldType::Genre => 2,
            FieldType::Synopsis => 3,
            FieldType::AltTitle => 4,
            FieldType::Author => 5,
            FieldType::Status => 6,
            FieldType::Similar => 7,
            FieldType::Chapters => 8,
        }
    }

    /// Whether this field has multiple nodes per page
    pub fn is_multi(&self) -> bool {
        matches!(self, FieldType::Genre | FieldType::Similar | FieldType::Chapters)
    }

    /// All field types in output order
    pub fn all() -> &'static [FieldType] {
        &[
            FieldType::Title, FieldType::Rating, FieldType::Genre, FieldType::Synopsis,
            FieldType::AltTitle, FieldType::Author, FieldType::Status,
            FieldType::Similar, FieldType::Chapters,
        ]
    }
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            FieldType::Title => "title",
            FieldType::Rating => "rating",
            FieldType::Genre => "genre",
            FieldType::Synopsis => "synopsis",
            FieldType::AltTitle => "alt_title",
            FieldType::Author => "author",
            FieldType::Status => "status",
            FieldType::Similar => "similar",
            FieldType::Chapters => "chapters",
        };
        write!(f, "{name}")
    }
}

/// Structural context for a DOM node, computed via tree walking
#[derive(Debug, Clone)]
struct NodeContext {
    depth: usize,
    sibling_index: usize,
    sibling_count: usize,
    child_count: usize,
    parent_tag: Option<String>,
    parent_classes: HashSet<String>,
    ancestor_classes: HashSet<String>,
    ancestor_ids: HashSet<String>,
    is_first_significant: bool,
    has_img_child: bool,
    bold_text_len: usize,
    total_text_len: usize,
    link_count: usize,
    bold_text_content: String,
    /// Document order index (set during walk, normalized after)
    doc_order: usize,
    /// Normalized document position (0=top, 1=bottom, set after walk)
    doc_position: f32,
}

/// Result of detecting a field in HTML
#[derive(Debug, Clone)]
pub struct DetectionResult {
    /// The detected text value
    pub text: String,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Tag name of the detected node
    pub tag_name: String,
    /// For <a> tags: the href value if present
    pub href: Option<String>,
}

/// Results for ALL fields from a single detection pass
#[derive(Debug, Clone, Default)]
pub struct AllFieldsResult {
    pub title: Option<DetectionResult>,
    pub rating: Option<DetectionResult>,
    pub genres: Vec<DetectionResult>,
    pub synopsis: Option<DetectionResult>,
    pub alt_title: Option<DetectionResult>,
    pub author: Option<DetectionResult>,
    pub status: Option<DetectionResult>,
    pub similar: Vec<DetectionResult>,
    pub chapters: Vec<DetectionResult>,
}

/// LM Detector Engine — single unified model for all fields
pub struct LmDetector {
    session: Session,
    /// Per-label optimal thresholds (loaded from model metadata JSON).
    /// Each field has a different optimal threshold computed from
    /// precision-recall curves on the validation set.
    thresholds: [f32; NUM_FIELDS],
}

impl LmDetector {
    /// Ensure the ONNX Runtime shared library can be found.
    ///
    /// `ort` with `load-dynamic` needs either:
    ///   1. `ORT_DYLIB_PATH` env var, or
    ///   2. The library on `LD_LIBRARY_PATH`
    ///
    /// This function checks common locations and sets `ORT_DYLIB_PATH`
    /// automatically if the library hasn't been located yet.
    fn ensure_ort_dylib() {
        if std::env::var("ORT_DYLIB_PATH").is_ok() {
            return; // Already set by the user
        }

        // Common locations for the ONNX Runtime shared library
        let candidates = [
            "/home/z/.local/lib/python3.13/site-packages/onnxruntime/capi/libonnxruntime.so.1.25.1",
            "/usr/lib/libonnxruntime.so",
            "/usr/local/lib/libonnxruntime.so",
        ];

        for path in &candidates {
            if std::path::Path::new(path).exists() {
                std::env::set_var("ORT_DYLIB_PATH", path);
                return;
            }
        }
    }

    /// Load per-label thresholds from model metadata JSON.
    /// Falls back to 0.5 for all fields if JSON is missing or invalid.
    fn load_thresholds(model_path: &std::path::Path) -> [f32; NUM_FIELDS] {
        let json_path = model_path.with_extension("json");
        let mut thresholds = [0.5f32; NUM_FIELDS];

        if let Ok(json_str) = std::fs::read_to_string(&json_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(ts) = meta.get("field_thresholds") {
                    for (i, field_name) in FIELD_NAMES_RUST.iter().enumerate() {
                        if let Some(t) = ts.get(field_name) {
                            if let Some(val) = t.as_f64() {
                                thresholds[i] = val as f32;
                            }
                        }
                    }
                    return thresholds;
                }
            }
        }

        thresholds
    }

    /// Create a new detector loading the unified multi-label model
    pub fn new() -> Result<Self> {
        Self::ensure_ort_dylib();

        let model_path = std::path::Path::new("models/field_detector.onnx");
        if !model_path.exists() {
            anyhow::bail!(
                "ONNX model not found at {}. Run lm/train_model.py --mode multilabel first.",
                model_path.display()
            );
        }
        let session = Session::builder()?
            .commit_from_file(model_path)?;
        let thresholds = Self::load_thresholds(model_path);
        Ok(Self { session, thresholds })
    }

    /// Create a detector with a custom model path
    pub fn from_path(model_path: &std::path::Path) -> Result<Self> {
        Self::ensure_ort_dylib();

        if !model_path.exists() {
            anyhow::bail!(
                "ONNX model not found at {}",
                model_path.display()
            );
        }
        let session = Session::builder()?
            .commit_from_file(model_path)?;
        let thresholds = Self::load_thresholds(model_path);
        Ok(Self { session, thresholds })
    }

    /// Detect ALL fields in HTML in a single pass (most efficient).
    ///
    /// DOM walk + feature extraction happens once, ONNX inference runs once
    /// per candidate node, and results are organized by field type.
    pub fn detect_all_fields(&mut self, html: &str) -> AllFieldsResult {
        let dom = match tl::parse(html, tl::ParserOptions::default()) {
            Ok(d) => d,
            Err(_) => return AllFieldsResult::default(),
        };
        let parser = dom.parser();

        // Step 1: Build node context map via DOM tree walking (1 pass)
        let contexts = Self::build_node_contexts(&dom, parser);

        // Step 2: Collect ALL candidate nodes with their features
        let mut candidates: Vec<(CandidateInfo, [f32; NUM_FEATURES])> = Vec::new();

        for tag_name in CANDIDATE_TAGS {
            if let Some(mut iter) = dom.query_selector(tag_name) {
                while let Some(handle) = iter.next() {
                    let node = match handle.get(parser) {
                        Some(n) => n,
                        None => continue,
                    };
                    let tag = match node.as_tag() {
                        Some(t) => t,
                        None => continue,
                    };

                    let name = tag.name().as_utf8_str();
                    if SKIP_TAGS.contains(&name.as_ref()) {
                        continue;
                    }

                    // Skip empty-text nodes (except <meta> which has content attribute)
                    let text = if name.as_ref() == "meta" {
                        tag.attributes()
                            .get(tl::Bytes::from("content"))
                            .flatten()
                            .map(|v| v.as_utf8_str().to_string())
                            .unwrap_or_default()
                    } else {
                        tag.inner_text(parser).trim().to_string()
                    };

                    if text.is_empty() {
                        continue;
                    }

                    // Look up structural context for this node
                    let node_id = handle.get_inner() as usize;
                    let ctx = contexts.get(&node_id);

                    // Extract features
                    let features = Self::extract_features(tag, parser, &name, ctx);

                    // Get href for <a> tags
                    let href = if name.as_ref() == "a" {
                        tag.attributes().get(tl::Bytes::from("href")).flatten()
                            .map(|v| v.as_utf8_str().to_string())
                    } else {
                        None
                    };

                    let info = CandidateInfo {
                        text,
                        tag_name: name.as_ref().to_string(),
                        href,
                    };

                    candidates.push((info, features));
                }
            }
        }

        if candidates.is_empty() {
            return AllFieldsResult::default();
        }

        // Step 3: Batch inference — run all candidates through the model at once
        let batch_size = candidates.len();
        let mut input_data = Vec::with_capacity(batch_size * NUM_FEATURES);
        for (_, features) in &candidates {
            input_data.extend_from_slice(features);
        }

        let input_array = ndarray::Array2::from_shape_vec(
            (batch_size, NUM_FEATURES),
            input_data,
        ).unwrap_or_else(|_| ndarray::Array2::zeros((1, NUM_FEATURES)));

        let input_tensor = ort::value::Tensor::from_array(input_array)
            .unwrap_or_else(|_| {
                ort::value::Tensor::from_array(ndarray::Array2::<f32>::zeros((1, NUM_FEATURES)))
                    .unwrap()
            });
        let input_value: ort::value::Value = input_tensor.into();

        let inputs = ort::inputs!["features" => input_value];

        let output = match self.session.run(inputs) {
            Ok(o) => o,
            Err(_) => return AllFieldsResult::default(),
        };

        // Extract probabilities: shape [batch_size, 9]
        let probs: Vec<f32> = match output["probabilities"]
            .try_extract_tensor::<f32>()
        {
            Ok((_, arr)) => arr.to_vec(),
            Err(_) => return AllFieldsResult::default(),
        };

        // Step 4: Organize results by field type using per-label thresholds
        let mut result = AllFieldsResult::default();

        // Track best for single-node fields
        let mut best_title: Option<(f32, CandidateInfo)> = None;
        let mut best_rating: Option<(f32, CandidateInfo)> = None;
        let mut best_synopsis: Option<(f32, CandidateInfo)> = None;
        let mut best_alt_title: Option<(f32, CandidateInfo)> = None;
        let mut best_author: Option<(f32, CandidateInfo)> = None;
        let mut best_status: Option<(f32, CandidateInfo)> = None;

        // Track all above-threshold for multi-node fields
        let mut genre_candidates: Vec<DetectionResult> = Vec::new();
        let mut similar_candidates: Vec<DetectionResult> = Vec::new();
        let mut chapter_candidates: Vec<DetectionResult> = Vec::new();

        for (i, (info, _)) in candidates.iter().enumerate() {
            let base = i * NUM_FIELDS;

            // Title (index 0)
            let title_prob = probs[base + 0];
            if title_prob > self.thresholds[0] {
                match &best_title {
                    Some((best, _)) if title_prob <= *best => {}
                    _ => best_title = Some((title_prob, info.clone())),
                }
            }

            // Rating (index 1)
            let rating_prob = probs[base + 1];
            if rating_prob > self.thresholds[1] {
                match &best_rating {
                    Some((best, _)) if rating_prob <= *best => {}
                    _ => best_rating = Some((rating_prob, info.clone())),
                }
            }

            // Genre (index 2) — multi-node
            let genre_prob = probs[base + 2];
            if genre_prob > self.thresholds[2] {
                genre_candidates.push(DetectionResult {
                    text: info.text.clone(),
                    confidence: genre_prob,
                    tag_name: info.tag_name.clone(),
                    href: info.href.clone(),
                });
            }

            // Synopsis (index 3)
            let synopsis_prob = probs[base + 3];
            if synopsis_prob > self.thresholds[3] {
                match &best_synopsis {
                    Some((best, _)) if synopsis_prob <= *best => {}
                    _ => best_synopsis = Some((synopsis_prob, info.clone())),
                }
            }

            // AltTitle (index 4)
            let alt_title_prob = probs[base + 4];
            if alt_title_prob > self.thresholds[4] {
                match &best_alt_title {
                    Some((best, _)) if alt_title_prob <= *best => {}
                    _ => best_alt_title = Some((alt_title_prob, info.clone())),
                }
            }

            // Author (index 5)
            let author_prob = probs[base + 5];
            if author_prob > self.thresholds[5] {
                match &best_author {
                    Some((best, _)) if author_prob <= *best => {}
                    _ => best_author = Some((author_prob, info.clone())),
                }
            }

            // Status (index 6)
            let status_prob = probs[base + 6];
            if status_prob > self.thresholds[6] {
                match &best_status {
                    Some((best, _)) if status_prob <= *best => {}
                    _ => best_status = Some((status_prob, info.clone())),
                }
            }

            // Similar (index 7) — multi-node
            let similar_prob = probs[base + 7];
            if similar_prob > self.thresholds[7] {
                similar_candidates.push(DetectionResult {
                    text: info.text.clone(),
                    confidence: similar_prob,
                    tag_name: info.tag_name.clone(),
                    href: info.href.clone(),
                });
            }

            // Chapters (index 8) — multi-node
            let chapters_prob = probs[base + 8];
            if chapters_prob > self.thresholds[8] {
                chapter_candidates.push(DetectionResult {
                    text: info.text.clone(),
                    confidence: chapters_prob,
                    tag_name: info.tag_name.clone(),
                    href: info.href.clone(),
                });
            }
        }

        // Build final results
        if let Some((prob, info)) = best_title {
            // Clean title: strip "Komik" prefix if present
            let cleaned = if info.text.to_lowercase().starts_with("komik") {
                let re = regex::Regex::new(r"(?i)^komik\s*").ok();
                re.map(|r| r.replace(&info.text, "").trim().to_string())
                    .unwrap_or_else(|| info.text.clone())
            } else {
                info.text.clone()
            };
            result.title = Some(DetectionResult {
                text: cleaned,
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        if let Some((prob, info)) = best_rating {
            result.rating = Some(DetectionResult {
                text: info.text.trim().to_string(),
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        if let Some((prob, info)) = best_synopsis {
            result.synopsis = Some(DetectionResult {
                text: info.text.trim().to_string(),
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        if let Some((prob, info)) = best_alt_title {
            // Extract value after the bold label (e.g., "Alternative:Nano Machine" → "Nano Machine")
            let cleaned = Self::extract_value_after_label(&info.text);
            result.alt_title = Some(DetectionResult {
                text: cleaned,
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        if let Some((prob, info)) = best_author {
            let cleaned = Self::extract_value_after_label(&info.text);
            result.author = Some(DetectionResult {
                text: cleaned,
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        if let Some((prob, info)) = best_status {
            let cleaned = Self::extract_value_after_label(&info.text);
            result.status = Some(DetectionResult {
                text: cleaned,
                confidence: prob,
                tag_name: info.tag_name,
                href: None,
            });
        }

        // Sort multi-node candidates by confidence (highest first)
        genre_candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        result.genres = genre_candidates;

        similar_candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        result.similar = similar_candidates;

        chapter_candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        result.chapters = chapter_candidates;

        result
    }

    /// Extract value after a colon from text like "Status:Berjalan" → "Berjalan"
    /// or "Alternative:Nano Machine - Nanogiga" → "Nano Machine - Nanogiga"
    fn extract_value_after_label(text: &str) -> String {
        // Try colon separator
        if let Some(pos) = text.find(':') {
            return text[pos + 1..].trim().to_string();
        }
        text.trim().to_string()
    }

    // ================================================================
    // Convenience methods (single-field, compatible with old API)
    // ================================================================

    /// Detect title in HTML
    pub fn detect_title(&mut self, html: &str) -> Option<String> {
        self.detect_all_fields(html).title.map(|r| r.text)
    }

    /// Detect rating in HTML (returns f64)
    pub fn detect_rating(&mut self, html: &str) -> Option<f64> {
        self.detect_all_fields(html).rating
            .and_then(|r| r.text.parse::<f64>().ok())
    }

    /// Detect all genre tags in HTML
    pub fn detect_genres(&mut self, html: &str) -> Vec<String> {
        self.detect_all_fields(html).genres
            .into_iter()
            .map(|r| r.text)
            .collect()
    }

    /// Detect synopsis text in HTML
    pub fn detect_synopsis(&mut self, html: &str) -> Option<String> {
        self.detect_all_fields(html).synopsis.map(|r| r.text)
    }

    /// Detect alt title in HTML
    pub fn detect_alt_title(&mut self, html: &str) -> Option<String> {
        self.detect_all_fields(html).alt_title.map(|r| r.text)
    }

    /// Detect author in HTML
    pub fn detect_author(&mut self, html: &str) -> Option<String> {
        self.detect_all_fields(html).author.map(|r| r.text)
    }

    /// Detect status in HTML
    pub fn detect_status(&mut self, html: &str) -> Option<String> {
        self.detect_all_fields(html).status.map(|r| r.text)
    }

    /// Detect similar komik links in HTML
    pub fn detect_similar(&mut self, html: &str) -> Vec<DetectionResult> {
        self.detect_all_fields(html).similar
    }

    /// Detect chapter links in HTML
    pub fn detect_chapters(&mut self, html: &str) -> Vec<DetectionResult> {
        self.detect_all_fields(html).chapters
    }

    /// Detect a specific field (for compatibility with per-field testing)
    pub fn detect(&mut self, field: FieldType, html: &str) -> Option<DetectionResult> {
        let results = self.detect_all_fields(html);
        match field {
            FieldType::Title => results.title,
            FieldType::Rating => results.rating,
            FieldType::Genre => results.genres.into_iter().next(),
            FieldType::Synopsis => results.synopsis,
            FieldType::AltTitle => results.alt_title,
            FieldType::Author => results.author,
            FieldType::Status => results.status,
            FieldType::Similar => results.similar.into_iter().next(),
            FieldType::Chapters => results.chapters.into_iter().next(),
        }
    }

    /// Detect all instances of a multi-node field
    pub fn detect_all(&mut self, field: FieldType, html: &str, threshold: f32) -> Vec<DetectionResult> {
        let results = self.detect_all_fields(html);
        let filtered = |v: Vec<DetectionResult>| v.into_iter()
            .filter(|r| r.confidence >= threshold)
            .collect();

        match field {
            FieldType::Genre => filtered(results.genres),
            FieldType::Similar => filtered(results.similar),
            FieldType::Chapters => filtered(results.chapters),
            FieldType::Title => results.title.into_iter().filter(|r| r.confidence >= threshold).collect(),
            FieldType::Rating => results.rating.into_iter().filter(|r| r.confidence >= threshold).collect(),
            FieldType::Synopsis => results.synopsis.into_iter().filter(|r| r.confidence >= threshold).collect(),
            FieldType::AltTitle => results.alt_title.into_iter().filter(|r| r.confidence >= threshold).collect(),
            FieldType::Author => results.author.into_iter().filter(|r| r.confidence >= threshold).collect(),
            FieldType::Status => results.status.into_iter().filter(|r| r.confidence >= threshold).collect(),
        }
    }

    // ================================================================
    // DOM Tree Walking — shared across all fields
    // ================================================================

    /// Build a map of node_id → NodeContext by walking the DOM tree.
    fn build_node_contexts(
        dom: &tl::VDom,
        parser: &tl::Parser,
    ) -> std::collections::HashMap<usize, NodeContext> {
        let mut contexts: std::collections::HashMap<usize, NodeContext> = std::collections::HashMap::new();
        let mut doc_order_counter: usize = 0;

        let top_children = dom.children();
        let sibling_count = top_children.len();

        for (sibling_idx, handle) in top_children.iter().enumerate() {
            let node = handle.get(parser);
            if let Some(node) = node {
                Self::walk_node(
                    node,
                    handle.get_inner() as usize,
                    parser,
                    0,
                    sibling_idx,
                    sibling_count,
                    None,
                    &HashSet::new(),
                    &HashSet::new(),
                    &mut contexts,
                    &mut doc_order_counter,
                );
            }
        }

        // Normalize doc_order to [0, 1] range for all nodes
        let max_order = doc_order_counter.max(1) as f32;
        for ctx in contexts.values_mut() {
            ctx.doc_position = ctx.doc_order as f32 / max_order;
        }

        contexts
    }

    /// Recursively walk a DOM node and its children, building context map.
    #[allow(clippy::too_many_arguments)]
    fn walk_node(
        node: &tl::Node,
        node_id: usize,
        parser: &tl::Parser,
        depth: usize,
        sibling_index: usize,
        sibling_count: usize,
        parent_info: Option<(&str, &HashSet<String>)>,
        ancestor_classes: &HashSet<String>,
        ancestor_ids: &HashSet<String>,
        contexts: &mut std::collections::HashMap<usize, NodeContext>,
        doc_order_counter: &mut usize,
    ) {
        let tag = match node.as_tag() {
            Some(t) => t,
            None => return,
        };

        let tag_name = tag.name().as_utf8_str();

        if SKIP_TAGS.contains(&tag_name.as_ref()) {
            return;
        }

        let node_classes = Self::get_classes_set(tag);

        // Get node ID attribute
        let node_id_attr = tag.attributes().get(tl::Bytes::from("id")).flatten()
            .map(|v| v.as_utf8_str().to_lowercase())
            .unwrap_or_default();

        let children_wrapper = tag.children();
        let children = children_wrapper.top();
        let child_count = children.len();

        let mut has_img_child = false;
        let mut bold_text_len = 0usize;
        let mut total_text_len = 0usize;
        let mut link_count = 0usize;
        let mut bold_text_parts: Vec<String> = Vec::new();

        for &child_handle in children.iter() {
            if let Some(child_node) = child_handle.get(parser) {
                if let Some(child_tag) = child_node.as_tag() {
                    let child_name = child_tag.name().as_utf8_str();
                    match child_name.as_ref() {
                        "img" => has_img_child = true,
                        "a" => link_count += 1,
                        "b" | "strong" => {
                            let inner = child_tag.inner_text(parser);
                            let t = inner.trim();
                            bold_text_len += t.len();
                            total_text_len += t.len();
                            bold_text_parts.push(t.to_lowercase());
                        }
                        _ => {
                            let inner = child_tag.inner_text(parser);
                            let t = inner.trim();
                            total_text_len += t.len();
                        }
                    }
                } else if let Some(raw) = child_node.as_raw() {
                    let raw_str = raw.as_utf8_str();
                    let t = raw_str.trim();
                    total_text_len += t.len();
                }
            }
        }

        if link_count == 0 && child_count > 0 {
            link_count = Self::count_links_recursive(tag, parser);
        }

        let full_inner = tag.inner_text(parser);
        let full_text = full_inner.trim();
        if total_text_len == 0 {
            total_text_len = full_text.len();
        }

        let parent_tag = parent_info.map(|(name, _)| name.to_string());
        let parent_classes = parent_info
            .map(|(_, classes)| classes.clone())
            .unwrap_or_default();

        let mut my_ancestor_classes = ancestor_classes.clone();
        if let Some((_, p_classes)) = parent_info {
            my_ancestor_classes.extend(p_classes.iter().cloned());
        }

        let mut my_ancestor_ids = ancestor_ids.clone();
        if !node_id_attr.is_empty() {
            my_ancestor_ids.insert(node_id_attr);
        }

        let is_first_significant = false;

        let bold_text_content = bold_text_parts.join(" ");

        let current_doc_order = *doc_order_counter;
        *doc_order_counter += 1;

        contexts.insert(
            node_id,
            NodeContext {
                depth,
                sibling_index,
                sibling_count,
                child_count,
                parent_tag,
                parent_classes,
                ancestor_classes: my_ancestor_classes.clone(),
                ancestor_ids: my_ancestor_ids.clone(),
                is_first_significant,
                has_img_child,
                bold_text_len,
                total_text_len,
                link_count,
                bold_text_content,
                doc_order: current_doc_order,
                doc_position: 0.0, // Will be normalized after the walk
            },
        );

        let child_count_val = children.len();
        for (child_idx, &child_handle) in children.iter().enumerate() {
            if let Some(child_node) = child_handle.get(parser) {
                Self::walk_node(
                    child_node,
                    child_handle.get_inner() as usize,
                    parser,
                    depth + 1,
                    child_idx,
                    child_count_val,
                    Some((&tag_name, &node_classes)),
                    &my_ancestor_classes,
                    &my_ancestor_ids,
                    contexts,
                    doc_order_counter,
                );
            }
        }
    }

    fn count_links_recursive(tag: &tl::HTMLTag, parser: &tl::Parser) -> usize {
        let mut count = 0usize;
        let children_wrapper = tag.children();
        let children = children_wrapper.top();
        for &child_handle in children.iter() {
            if let Some(child_node) = child_handle.get(parser) {
                if let Some(child_tag) = child_node.as_tag() {
                    let name = child_tag.name().as_utf8_str();
                    if name.as_ref() == "a" {
                        count += 1;
                    }
                    count += Self::count_links_recursive(child_tag, parser);
                }
            }
        }
        count
    }

    fn get_classes_set(tag: &tl::HTMLTag) -> HashSet<String> {
        tag.attributes()
            .class_iter()
            .map(|iter| iter.map(|s| s.to_lowercase()).collect::<HashSet<String>>())
            .unwrap_or_default()
    }

    // ================================================================
    // Feature Extraction — 40 dims, shared across all fields
    // ================================================================

    /// Extract 40-dim feature vector from a DOM tag with full structural context.
    pub fn extract_features(
        tag: &tl::HTMLTag,
        parser: &tl::Parser,
        tag_name: &str,
        ctx: Option<&NodeContext>,
    ) -> [f32; NUM_FEATURES] {
        let mut feat = [0.0f32; NUM_FEATURES];

        // [0-8] Tag one-hot
        for (i, vocab_tag) in TAG_VOCAB.iter().enumerate() {
            if tag_name == *vocab_tag {
                feat[i] = 1.0;
                break;
            }
        }

        // Extract class info
        let classes_lower: Vec<String> = tag
            .attributes()
            .class_iter()
            .map(|iter| iter.map(|s| s.to_lowercase()).collect::<Vec<_>>())
            .unwrap_or_default();
        let classes_joined = classes_lower.join(" ");

        // [9-13] Class features
        feat[9] = if classes_joined.contains("title") { 1.0 } else { 0.0 };
        feat[10] = if classes_joined.contains("entry") { 1.0 } else { 0.0 };
        feat[11] = if classes_joined.contains("info") { 1.0 } else { 0.0 };
        feat[12] = if classes_joined.contains("rating") { 1.0 } else { 0.0 };
        feat[13] = if classes_joined.contains("archive") { 1.0 } else { 0.0 };

        // [14] Has ID
        let id_val = tag.attributes().get(tl::Bytes::from("id")).flatten();
        feat[14] = if id_val.is_some() { 1.0 } else { 0.0 };

        // [15-19] Structural features (from DOM tree context)
        if let Some(ctx) = ctx {
            feat[15] = (ctx.depth as f32 / 20.0).min(1.0);
            feat[16] = (ctx.sibling_index as f32 / 20.0).min(1.0);
            feat[17] = (ctx.sibling_count as f32 / 20.0).min(1.0);
            feat[18] = (ctx.child_count as f32 / 50.0).min(1.0);

            // [21] Parent is div
            feat[21] = if ctx.parent_tag.as_deref() == Some("div") { 1.0 } else { 0.0 };

            // [22] Parent has class containing "info"
            feat[22] = if ctx.parent_classes.iter().any(|c| c.contains("info")) { 1.0 } else { 0.0 };

            // [25-27] Ancestor features
            feat[25] = if ctx.ancestor_classes.contains("spe") { 1.0 } else { 0.0 };
            feat[26] = if ctx.ancestor_classes.contains("infox") { 1.0 } else { 0.0 };
            feat[27] = if ctx.ancestor_classes.contains("infoanime") { 1.0 } else { 0.0 };

            // [28] Bold text ratio
            feat[28] = if ctx.total_text_len > 0 {
                ctx.bold_text_len as f32 / ctx.total_text_len as f32
            } else {
                0.0
            };

            // [29] Link count
            feat[29] = (ctx.link_count as f32 / 10.0).min(1.0);

            // [32-34] Bold text label patterns
            feat[32] = if ctx.bold_text_content.contains("status") { 1.0 } else { 0.0 };
            feat[33] = if ctx.bold_text_content.contains("pengarang") || ctx.bold_text_content.contains("author") { 1.0 } else { 0.0 };
            feat[34] = if ctx.bold_text_content.contains("alternative") || ctx.bold_text_content.contains("alternatif") { 1.0 } else { 0.0 };

            // [37] Ancestor: mirip/bxcl containers
            feat[37] = if ctx.ancestor_ids.contains("mirip") ||
                          ctx.ancestor_classes.contains("mirip") ||
                          ctx.ancestor_classes.contains("bxcl") ||
                          ctx.ancestor_ids.contains("chapter_list") { 1.0 } else { 0.0 };
        } else {
            feat[15] = 0.0;
            feat[16] = 0.0;
            feat[17] = 0.3;
            feat[18] = 0.0;
            feat[21] = 0.0;
            feat[22] = 0.0;
            feat[25] = 0.0;
            feat[26] = 0.0;
            feat[27] = 0.0;
            feat[28] = 0.0;
            feat[29] = 0.0;
            feat[32] = 0.0;
            feat[33] = 0.0;
            feat[34] = 0.0;
            feat[37] = 0.0;
        }

        // [19] Text length — log-transformed for better MLP convergence
        // Raw text_length has heavy-tail distribution, log(1+x) compresses range
        let text = tag.inner_text(parser).trim().to_string();
        let text_len = text.len() as f32;
        feat[19] = (text_len.ln_1p() / 6.0).min(1.0);

        // [20] Text starts with "Komik"
        feat[20] = if text.to_lowercase().starts_with("komik") { 1.0 } else { 0.0 };

        // [23] Has itemprop attribute
        let itemprop_val = tag.attributes().get(tl::Bytes::from("itemprop")).flatten();
        feat[23] = if itemprop_val.is_some() { 1.0 } else { 0.0 };

        // [24] itemprop value is "ratingValue"
        feat[24] = if itemprop_val
            .map(|v| v.try_as_utf8_str().unwrap_or("") == "ratingValue")
            .unwrap_or(false) { 1.0 } else { 0.0 };

        // [30] Has rel=tag
        let has_rel_tag = tag.attributes().get(tl::Bytes::from("rel")).flatten()
            .map(|v| v.try_as_utf8_str().unwrap_or("").contains("tag"))
            .unwrap_or(false);
        feat[30] = if has_rel_tag { 1.0 } else { 0.0 };

        // [31] Font size indicator
        let font_map = [
            ("h1", 1.0), ("h2", 0.8), ("h3", 0.6),
            ("h4", 0.5), ("h5", 0.4), ("h6", 0.3),
        ];
        feat[31] = font_map.iter()
            .find(|(name, _)| *name == tag_name)
            .map(|(_, v)| *v)
            .unwrap_or(0.0);

        // [35] Class: desc/synopsis/entry-content
        feat[35] = if classes_lower.iter().any(|c| c == "desc" || c == "synopsis" || c == "entry-content") { 1.0 } else { 0.0 };

        // [36] href contains "-chapter-"
        let href = tag.attributes().get(tl::Bytes::from("href")).flatten()
            .map(|v| v.as_utf8_str().to_lowercase())
            .unwrap_or_default();
        feat[36] = if href.contains("-chapter-") { 1.0 } else { 0.0 };

        // [38] Class: lchx/series
        feat[38] = if classes_lower.iter().any(|c| c == "lchx" || c == "series") { 1.0 } else { 0.0 };

        // [39] Text contains "Chapter"
        feat[39] = if text.to_lowercase().contains("chapter") { 1.0 } else { 0.0 };

        // === REGEX NUMERIC FEATURES (40-42) ===

        // [40] Contains rating float — e.g. "7.5", "8.0", "9.5"
        // More specific than itemprop: detects the numeric rating value pattern itself,
        // which works even without semantic markup.
        feat[40] = if rating_float_re().is_match(&text) { 1.0 } else { 0.0 };

        // [41] Contains chapter number — e.g. "Chapter 1", "Bab 45"
        // More specific than text_contains_chapter (39): requires a NUMBER after
        // the keyword, distinguishing actual chapter links from the word "chapter"
        // appearing in synopsis text.
        feat[41] = if chapter_number_re().is_match(&text) { 1.0 } else { 0.0 };

        // [42] Contains 4-digit year — e.g. "2024", "2019"
        // Useful for distinguishing metadata (author, status) from other content.
        feat[42] = if year_re().is_match(&text) { 1.0 } else { 0.0 };

        // === DOM POSITIONAL PRIOR (43) ===

        // [43] Normalized document position (0=top, 1=bottom)
        // Title is typically near the top (~0.0-0.2), synopsis in the middle (~0.3-0.6),
        // chapters near the bottom (~0.7-1.0). This positional prior helps the MLP
        // learn field location patterns.
        feat[43] = ctx.map(|c| c.doc_position).unwrap_or(0.0);

        feat
    }
}

/// Internal struct for candidate node info during detection
#[derive(Debug, Clone)]
struct CandidateInfo {
    text: String,
    tag_name: String,
    href: Option<String>,
}
