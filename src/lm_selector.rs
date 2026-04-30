/// LM-based Selector Engine — Title Detection Experiment
///
/// Uses a small ONNX model to detect which DOM node
/// contains the komik title, replacing hardcoded CSS selectors.
///
/// Architecture:
///   HTML → DOM Walk → Feature Vector (30 dims per node) → ONNX Inference → Title Node
///
/// Feature vector (30 dims):
///   [0-6]   Tag one-hot: h1, h2, h3, span, div, a, td
///   [7-9]   Class contains: title, entry, info
///   [10]    Has non-empty id
///   [11-15] Structural: depth, sibling_index, sibling_count, child_count, text_length
///   [16]    Text starts with "Komik"
///   [17-18] Parent: is_div, has_info_class
///   [19]    Has itemprop attribute
///   [20-22] Inside ancestor: spe, infox, infoanime
///   [23]    Bold text ratio
///   [24-25] Link count, has rel=tag
///   [26-29] Word count, has_img_child, is_first_significant, font_size_indicator

use anyhow::Result;
use ort::session::Session;
use std::collections::{HashMap, HashSet};

/// Number of features per DOM node
const NUM_FEATURES: usize = 30;

/// Tag vocabulary for one-hot encoding
const TAG_VOCAB: &[&str] = &["h1", "h2", "h3", "span", "div", "a", "td"];

/// Tags to skip during tree walking
const SKIP_TAGS: &[&str] = &["script", "style", "noscript", "meta", "link", "head"];

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

/// LM Selector Engine for title detection
pub struct LmTitleDetector {
    session: Session,
}

impl LmTitleDetector {
    /// Load ONNX model from file
    pub fn new(model_path: &std::path::Path) -> Result<Self> {
        let session = Session::builder()?
            .commit_from_file(model_path)?;
        Ok(Self { session })
    }

    /// Load from embedded default path (models/title_detector.onnx)
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

    /// Detect title text from HTML using the LM model.
    ///
    /// Strategy: Build DOM tree context, walk all candidate tags,
    /// extract 30-dim features for each, run ONNX inference,
    /// and return the text of the node with highest probability.
    pub fn detect_title(&mut self, html: &str) -> Option<String> {
        let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
        let parser = dom.parser();

        // Step 1: Build node context map via DOM tree walking
        let contexts = Self::build_node_contexts(&dom, parser);

        // Step 2: Collect candidate nodes — tags that could be a title
        let mut candidates: Vec<(f32, String)> = Vec::new();

        for tag_name in TAG_VOCAB {
            if let Some(mut iter) = dom.query_selector(tag_name) {
                while let Some(handle) = iter.next() {
                    let node = handle.get(parser)?;
                    let tag = node.as_tag()?;

                    let name = tag.name().as_utf8_str();
                    if SKIP_TAGS.contains(&name.as_ref()) {
                        continue;
                    }

                    let text = tag.inner_text(parser).trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    // Look up structural context for this node
                    let node_id = handle.get_inner() as usize;
                    let ctx = contexts.get(&node_id);

                    // Extract features with real structural data
                    let features = Self::extract_features(tag, parser, &name, ctx);
                    let prob = self.infer(&features).unwrap_or(0.0);

                    if prob > 0.01 {
                        candidates.push((prob, text));
                    }
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }

        // Find node with highest probability
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let (best_prob, best_text) = &candidates[0];

        // Threshold: only return if confidence > 0.5
        if *best_prob > 0.5 {
            // Clean title: strip "Komik" prefix if present
            let cleaned = best_text.trim();
            let cleaned = if cleaned.to_lowercase().starts_with("komik") {
                let re = regex::Regex::new(r"(?i)^komik\s*").ok();
                re.map(|r| r.replace(cleaned, "").trim().to_string())
                    .unwrap_or_else(|| cleaned.to_string())
            } else {
                cleaned.to_string()
            };
            Some(cleaned)
        } else {
            None
        }
    }

    /// Build a map of node_id → NodeContext by walking the DOM tree.
    ///
    /// Uses `tl`'s `children().top()` API to traverse the tree recursively,
    /// computing depth, parent, sibling, and ancestor info for each tag node.
    fn build_node_contexts(
        dom: &tl::VDom,
        parser: &tl::Parser,
    ) -> HashMap<usize, NodeContext> {
        let mut contexts: HashMap<usize, NodeContext> = HashMap::new();

        // Walk top-level children of the document
        let top_children = dom.children();
        let sibling_count = top_children.len();

        for (sibling_idx, handle) in top_children.iter().enumerate() {
            let node = handle.get(parser);
            if let Some(node) = node {
                Self::walk_node(
                    node,
                    handle.get_inner() as usize,
                    parser,
                    0,          // depth: top-level = 0
                    sibling_idx,
                    sibling_count,
                    None,       // no parent at top level
                    &HashSet::new(), // no ancestor classes at top level
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
            None => return, // Skip raw text and comment nodes
        };

        let tag_name = tag.name().as_utf8_str();

        // Skip script/style nodes
        if SKIP_TAGS.contains(&tag_name.as_ref()) {
            return;
        }

        // Extract this node's classes
        let node_classes = Self::get_classes_set(tag);

        // Compute child count and analyze children
        let children_wrapper = tag.children();
        let children = children_wrapper.top();
        let child_count = children.len();

        // Analyze children for: img child, bold text, links
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

        // Also count links recursively inside non-<a> children
        // For link_count feature (matches Python's find_all('a'))
        if link_count == 0 && child_count > 0 {
            link_count = Self::count_links_recursive(tag, parser);
        }

        // Add this node's total text length (from inner_text)
        let full_inner = tag.inner_text(parser);
        let full_text = full_inner.trim();
        if total_text_len == 0 {
            total_text_len = full_text.len();
        }

        // Compute parent info
        let parent_tag = parent_info.map(|(name, _)| name.to_string());
        let parent_classes = parent_info
            .map(|(_, classes)| classes.clone())
            .unwrap_or_default();

        // Build ancestor classes: parent's ancestors + parent's own classes
        let mut my_ancestor_classes = ancestor_classes.clone();
        if let Some((_, p_classes)) = parent_info {
            my_ancestor_classes.extend(p_classes.iter().cloned());
        }

        // Compute is_first_significant: first text-bearing node among siblings
        // (We approximate: this is set during sibling iteration below)
        let is_first_significant = false; // Will be updated after sibling scan

        // Store context
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

        // Walk children recursively
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

    /// Count all <a> tags recursively inside a tag
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
                    // Recurse into non-leaf children
                    count += Self::count_links_recursive(child_tag, parser);
                }
            }
        }
        count
    }

    /// Extract classes from a tag into a HashSet
    fn get_classes_set(tag: &tl::HTMLTag) -> HashSet<String> {
        tag.attributes()
            .class_iter()
            .map(|iter| iter.map(|s| s.to_lowercase()).collect::<HashSet<String>>())
            .unwrap_or_default()
    }

    /// Extract 30-dim feature vector from a DOM tag with full structural context.
    ///
    /// Feature parity with Python training pipeline (extract_features.py).
    fn extract_features(
        tag: &tl::HTMLTag,
        parser: &tl::Parser,
        tag_name: &str,
        ctx: Option<&NodeContext>,
    ) -> [f32; NUM_FEATURES] {
        let mut feat = [0.0f32; NUM_FEATURES];

        // [0-6] Tag one-hot
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

        // [7-9] Class features
        feat[7] = if classes_joined.contains("title") { 1.0 } else { 0.0 };
        feat[8] = if classes_joined.contains("entry") { 1.0 } else { 0.0 };
        feat[9] = if classes_joined.contains("info") { 1.0 } else { 0.0 };

        // [10] Has ID
        let id_val = tag.attributes().get(tl::Bytes::from("id")).flatten();
        feat[10] = if id_val.is_some() { 1.0 } else { 0.0 };

        // [11-15] Structural features (from DOM tree context)
        if let Some(ctx) = ctx {
            // [11] Depth (normalized: /20.0, capped at 1.0)
            feat[11] = (ctx.depth as f32 / 20.0).min(1.0);

            // [12] Sibling index (normalized: /20.0, capped at 1.0)
            feat[12] = (ctx.sibling_index as f32 / 20.0).min(1.0);

            // [13] Sibling count (normalized: /20.0, capped at 1.0)
            feat[13] = (ctx.sibling_count as f32 / 20.0).min(1.0);

            // [14] Child count (normalized: /50.0, capped at 1.0)
            feat[14] = (ctx.child_count as f32 / 50.0).min(1.0);

            // [17] Parent is div
            feat[17] = if ctx.parent_tag.as_deref() == Some("div") { 1.0 } else { 0.0 };

            // [18] Parent has class containing "info"
            feat[18] = if ctx.parent_classes.iter().any(|c| c.contains("info")) { 1.0 } else { 0.0 };

            // [20-22] Ancestor features (check ancestor classes)
            feat[20] = if ctx.ancestor_classes.contains("spe") { 1.0 } else { 0.0 };
            feat[21] = if ctx.ancestor_classes.contains("infox") { 1.0 } else { 0.0 };
            feat[22] = if ctx.ancestor_classes.contains("infoanime") { 1.0 } else { 0.0 };

            // [23] Bold text ratio
            feat[23] = if ctx.total_text_len > 0 {
                ctx.bold_text_len as f32 / ctx.total_text_len as f32
            } else {
                0.0
            };

            // [24] Link count (normalized: /10.0, capped at 1.0)
            feat[24] = (ctx.link_count as f32 / 10.0).min(1.0);

            // [27] Has img child
            feat[27] = if ctx.has_img_child { 1.0 } else { 0.0 };

            // [28] Is first significant
            feat[28] = if ctx.is_first_significant { 1.0 } else { 0.0 };
        } else {
            // Fallback: no context available (shouldn't happen normally)
            feat[11] = 0.0; // depth
            feat[12] = 0.0; // sibling_index
            feat[13] = 0.3; // sibling_count (approximate)
            feat[14] = 0.0; // child_count
            feat[17] = 0.0; // parent_is_div
            feat[18] = 0.0; // parent_has_info
            feat[20] = 0.0; // is_inside_spe
            feat[21] = 0.0; // is_inside_infox
            feat[22] = 0.0; // is_inside_infoanime
            feat[23] = 0.0; // bold_text_ratio
            feat[24] = 0.0; // link_count
            feat[27] = 0.0; // has_img_child
            feat[28] = 0.0; // is_first_significant
        }

        // [15] Text length (normalized: /200.0, capped at 1.0)
        let text = tag.inner_text(parser).trim().to_string();
        feat[15] = (text.len() as f32 / 200.0).min(1.0);

        // [16] Text starts with "Komik"
        feat[16] = if text.to_lowercase().starts_with("komik") { 1.0 } else { 0.0 };

        // [19] Has itemprop attribute
        let has_itemprop = tag.attributes().get(tl::Bytes::from("itemprop")).flatten().is_some();
        feat[19] = if has_itemprop { 1.0 } else { 0.0 };

        // [25] Has rel=tag
        let has_rel_tag = tag.attributes().get(tl::Bytes::from("rel")).flatten()
            .map(|v| v.try_as_utf8_str().unwrap_or("").contains("tag"))
            .unwrap_or(false);
        feat[25] = if has_rel_tag { 1.0 } else { 0.0 };

        // [26] Word count (normalized: /20.0, capped at 1.0)
        feat[26] = (text.split_whitespace().count() as f32 / 20.0).min(1.0);

        // [29] Font size indicator
        let font_map = [
            ("h1", 1.0), ("h2", 0.8), ("h3", 0.6),
            ("h4", 0.5), ("h5", 0.4), ("h6", 0.3),
        ];
        feat[29] = font_map.iter()
            .find(|(name, _)| *name == tag_name)
            .map(|(_, v)| *v)
            .unwrap_or(0.0);

        feat
    }

    /// Run ONNX inference on a single feature vector
    fn infer(&mut self, features: &[f32; NUM_FEATURES]) -> Result<f32> {
        let input_array = ndarray::Array2::from_shape_vec(
            (1, NUM_FEATURES),
            features.to_vec(),
        )?;

        let input_tensor = ort::value::Tensor::from_array(input_array)?;
        let input_value: ort::value::Value = input_tensor.into();

        let inputs = ort::inputs!["features" => input_value];

        let output = self.session.run(inputs)?;
        let prob: f32 = output["probability"]
            .try_extract_tensor::<f32>()?
            .1
            .first()
            .copied()
            .unwrap_or(0.0);

        Ok(prob)
    }
}
