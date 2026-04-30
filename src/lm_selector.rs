/// LM-based Selector Engine — Multi-Field Detection
///
/// Uses small ONNX models to detect which DOM nodes contain
/// specific fields (title, rating, etc.), replacing hardcoded CSS selectors.
///
/// Architecture:
///   HTML → DOM Walk → Feature Vector (per node) → ONNX Inference → Field Node
///
/// The DOM tree walking and feature extraction is shared across all fields.
/// Each field has its own trained ONNX model (~17KB) that takes the same
/// feature vector and outputs a probability that the node contains that field.
///
/// Feature vector (32 dims):
///   [0-8]   Tag one-hot: h1, h2, h3, span, div, a, td, i, meta
///   [9-13]  Class contains: title, entry, info, rating, archive
///   [14]    Has non-empty id
///   [15-19] Structural: depth, sibling_index, sibling_count, child_count, text_length
///   [20]    Text starts with "Komik"
///   [21-22] Parent: is_div, has_info_class
///   [23-24] Attribute: has_itemprop, itemprop_value (ratingValue=1, else 0)
///   [25-27] Inside ancestor: spe, infox, infoanime
///   [28]    Bold text ratio
///   [29-30] Link count, has rel=tag
///   [31]    Font size indicator

use anyhow::Result;
use ort::session::Session;
use std::collections::{HashMap, HashSet};

/// Number of features per DOM node
pub const NUM_FEATURES: usize = 32;

/// Tag vocabulary for one-hot encoding
const TAG_VOCAB: &[&str] = &["h1", "h2", "h3", "span", "div", "a", "td", "i", "meta"];

/// Tags to skip during tree walking
const SKIP_TAGS: &[&str] = &["script", "style", "noscript", "head"];

/// Tags that are candidates for ANY field detection
const CANDIDATE_TAGS: &[&str] = &[
    "h1", "h2", "h3", "span", "div", "a", "td", "i", "meta",
    "p", "b", "strong", "label", "section", "article",
];

/// Which fields we can detect with LM
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    Title,
    Rating,
    Genre,
    Synopsis,
}

impl FieldType {
    /// Default model path for this field type
    pub fn model_path(&self) -> &'static str {
        match self {
            FieldType::Title => "models/title_detector.onnx",
            FieldType::Rating => "models/rating_detector.onnx",
            FieldType::Genre => "models/genre_detector.onnx",
            FieldType::Synopsis => "models/synopsis_detector.onnx",
        }
    }

    /// Input name in the ONNX model
    pub fn input_name(&self) -> &'static str {
        "features"
    }

    /// Output name in the ONNX model
    pub fn output_name(&self) -> &'static str {
        "probability"
    }

    /// Whether this field has multiple nodes per page
    pub fn is_multi(&self) -> bool {
        matches!(self, FieldType::Genre)
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
    is_first_significant: bool,
    has_img_child: bool,
    bold_text_len: usize,
    total_text_len: usize,
    link_count: usize,
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
}

/// LM Detector Engine — supports multiple field types
pub struct LmDetector {
    sessions: HashMap<FieldType, Session>,
}

impl LmDetector {
    /// Create a new detector loading models for the specified field types
    pub fn new(field_types: &[FieldType]) -> Result<Self> {
        let mut sessions = HashMap::new();
        for ft in field_types {
            let path = ft.model_path();
            let model_path = std::path::Path::new(path);
            if !model_path.exists() {
                anyhow::bail!(
                    "ONNX model not found at {}. Run lm/train_model.py first.",
                    path
                );
            }
            let session = Session::builder()?
                .commit_from_file(model_path)?;
            sessions.insert(*ft, session);
        }
        Ok(Self { sessions })
    }

    /// Create a detector for title field only (backward compatible)
    pub fn new_title_only() -> Result<Self> {
        Self::new(&[FieldType::Title])
    }

    /// Create a detector for all supported fields
    pub fn new_all_fields() -> Result<Self> {
        Self::new(&[FieldType::Title, FieldType::Rating, FieldType::Genre, FieldType::Synopsis])
    }

    /// Detect a specific field in HTML (single best result)
    pub fn detect(&mut self, field: FieldType, html: &str) -> Option<DetectionResult> {
        let session = self.sessions.get_mut(&field)?;

        let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
        let parser = dom.parser();

        // Step 1: Build node context map via DOM tree walking
        let contexts = Self::build_node_contexts(&dom, parser);

        // Step 2: Collect candidate nodes
        let mut candidates: Vec<(f32, String, String)> = Vec::new(); // (prob, text, tag_name)

        for tag_name in CANDIDATE_TAGS {
            if let Some(mut iter) = dom.query_selector(tag_name) {
                while let Some(handle) = iter.next() {
                    let node = handle.get(parser)?;
                    let tag = node.as_tag()?;

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

                    // Extract features with real structural data
                    let features = Self::extract_features(tag, parser, &name, ctx);
                    let prob = Self::infer_session(session, &features).unwrap_or(0.0);

                    if prob > 0.01 {
                        candidates.push((prob, text, name.as_ref().to_string()));
                    }
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }

        // Find node with highest probability
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let (best_prob, best_text, best_tag) = &candidates[0];

        // Threshold: only return if confidence > 0.5
        if *best_prob > 0.5 {
            let cleaned_text = match field {
                FieldType::Title => {
                    // Clean title: strip "Komik" prefix if present
                    let cleaned = best_text.trim();
                    if cleaned.to_lowercase().starts_with("komik") {
                        let re = regex::Regex::new(r"(?i)^komik\s*").ok();
                        re.map(|r| r.replace(cleaned, "").trim().to_string())
                            .unwrap_or_else(|| cleaned.to_string())
                    } else {
                        cleaned.to_string()
                    }
                }
                FieldType::Rating | FieldType::Genre | FieldType::Synopsis => {
                    // Clean: just trim whitespace
                    best_text.trim().to_string()
                }
            };

            Some(DetectionResult {
                text: cleaned_text,
                confidence: *best_prob,
                tag_name: best_tag.clone(),
            })
        } else {
            None
        }
    }

    /// Convenience method: detect title
    pub fn detect_title(&mut self, html: &str) -> Option<String> {
        self.detect(FieldType::Title, html).map(|r| r.text)
    }

    /// Convenience method: detect rating (returns f64)
    pub fn detect_rating(&mut self, html: &str) -> Option<f64> {
        self.detect(FieldType::Rating, html)
            .and_then(|r| r.text.parse::<f64>().ok())
    }

    /// Detect all instances of a multi-node field in HTML.
    /// Returns all nodes above threshold, sorted by confidence (highest first).
    pub fn detect_all(&mut self, field: FieldType, html: &str, threshold: f32) -> Vec<DetectionResult> {
        let session = match self.sessions.get_mut(&field) {
            Some(s) => s,
            None => return Vec::new(),
        };

        let dom = match tl::parse(html, tl::ParserOptions::default()) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let parser = dom.parser();

        // Step 1: Build node context map via DOM tree walking
        let contexts = Self::build_node_contexts(&dom, parser);

        // Step 2: Collect candidate nodes above threshold
        let mut candidates: Vec<DetectionResult> = Vec::new();

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

                    // Skip empty-text nodes
                    let text = tag.inner_text(parser).trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    // Look up structural context
                    let node_id = handle.get_inner() as usize;
                    let ctx = contexts.get(&node_id);

                    // Extract features and run inference
                    let features = Self::extract_features(tag, parser, &name, ctx);
                    let prob = Self::infer_session(session, &features).unwrap_or(0.0);

                    if prob > threshold {
                        let cleaned_text = text.trim().to_string();
                        candidates.push(DetectionResult {
                            text: cleaned_text,
                            confidence: prob,
                            tag_name: name.as_ref().to_string(),
                        });
                    }
                }
            }
        }

        // Sort by confidence (highest first)
        candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        candidates
    }

    /// Convenience method: detect all genre tags
    pub fn detect_genres(&mut self, html: &str) -> Vec<String> {
        self.detect_all(FieldType::Genre, html, 0.5)
            .into_iter()
            .map(|r| r.text)
            .collect()
    }

    /// Convenience method: detect synopsis text
    pub fn detect_synopsis(&mut self, html: &str) -> Option<String> {
        self.detect(FieldType::Synopsis, html).map(|r| r.text)
    }

    // ================================================================
    // DOM Tree Walking — shared across all fields
    // ================================================================

    /// Build a map of node_id → NodeContext by walking the DOM tree.
    fn build_node_contexts(
        dom: &tl::VDom,
        parser: &tl::Parser,
    ) -> HashMap<usize, NodeContext> {
        let mut contexts: HashMap<usize, NodeContext> = HashMap::new();

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
                    &mut contexts,
                );
            }
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
        contexts: &mut HashMap<usize, NodeContext>,
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

        let children_wrapper = tag.children();
        let children = children_wrapper.top();
        let child_count = children.len();

        let mut has_img_child = false;
        let mut bold_text_len = 0usize;
        let mut total_text_len = 0usize;
        let mut link_count = 0usize;

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

        let is_first_significant = false;

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
                is_first_significant,
                has_img_child,
                bold_text_len,
                total_text_len,
                link_count,
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
                    contexts,
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
    // Feature Extraction — 32 dims, shared across all fields
    // ================================================================

    /// Extract 32-dim feature vector from a DOM tag with full structural context.
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
        }

        // [19] Text length (normalized: /200.0, capped at 1.0)
        let text = tag.inner_text(parser).trim().to_string();
        feat[19] = (text.len() as f32 / 200.0).min(1.0);

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

        feat
    }

    /// Run ONNX inference on a single feature vector using a specific session
    fn infer_session(session: &mut Session, features: &[f32; NUM_FEATURES]) -> Result<f32> {
        let input_array = ndarray::Array2::from_shape_vec(
            (1, NUM_FEATURES),
            features.to_vec(),
        )?;

        let input_tensor = ort::value::Tensor::from_array(input_array)?;
        let input_value: ort::value::Value = input_tensor.into();

        let inputs = ort::inputs!["features" => input_value];

        let output = session.run(inputs)?;
        let prob: f32 = output["probability"]
            .try_extract_tensor::<f32>()?
            .1
            .first()
            .copied()
            .unwrap_or(0.0);

        Ok(prob)
    }
}

// ================================================================
// Backward-compatible LmTitleDetector wrapper
// ================================================================

/// Legacy wrapper for title-only detection (backward compatible)
pub struct LmTitleDetector {
    inner: LmDetector,
}

impl LmTitleDetector {
    pub fn new(model_path: &std::path::Path) -> Result<Self> {
        // Load the model from the specified path
        let session = Session::builder()?
            .commit_from_file(model_path)?;
        let mut sessions = HashMap::new();
        sessions.insert(FieldType::Title, session);
        Ok(Self {
            inner: LmDetector { sessions },
        })
    }

    pub fn new_default() -> Result<Self> {
        let model_path = std::path::Path::new("models/title_detector.onnx");
        if !model_path.exists() {
            anyhow::bail!(
                "ONNX model not found at {}. Run lm/train_model.py first.",
                model_path.display()
            );
        }
        Self::new(model_path)
    }

    pub fn detect_title(&mut self, html: &str) -> Option<String> {
        self.inner.detect_title(html)
    }
}
