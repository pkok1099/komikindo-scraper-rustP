/// LM-based Selector Engine — Title Detection Experiment
///
/// Uses a small ONNX model (17KB) to detect which DOM node
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

/// Number of features per DOM node
const NUM_FEATURES: usize = 30;

/// Tag vocabulary for one-hot encoding
const TAG_VOCAB: &[&str] = &["h1", "h2", "h3", "span", "div", "a", "td"];

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
    /// Strategy: Use query_selector to find candidate tags (h1, h2, span, div, a),
    /// extract features for each, run ONNX inference, and return the text
    /// of the node with highest probability.
    pub fn detect_title(&mut self, html: &str) -> Option<String> {
        let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
        let parser = dom.parser();

        // Collect candidate nodes — all tags that could be a title
        let mut candidates: Vec<(f32, String)> = Vec::new();

        // Walk all nodes via query_selector for each tag type
        for tag_name in TAG_VOCAB {
            let sel = format!("{tag_name}");
            if let Some(mut iter) = dom.query_selector(&sel) {
                while let Some(handle) = iter.next() {
                    let node = handle.get(parser)?;
                    let tag = node.as_tag()?;

                    // Skip script/style
                    let name = tag.name().as_utf8_str();
                    if matches!(name.as_ref(), "script" | "style" | "noscript" | "meta" | "link" | "head") {
                        continue;
                    }

                    let text = tag.inner_text(parser).trim().to_string();
                    if text.is_empty() {
                        continue;
                    }

                    // Extract features
                    let features = self.extract_features(tag, parser, &name);
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

    /// Extract 30-dim feature vector from a DOM tag
    fn extract_features(
        &self,
        tag: &tl::HTMLTag,
        parser: &tl::Parser,
        tag_name: &str,
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
        let classes: Vec<&str> = tag.attributes()
            .class_iter()
            .map(|iter| iter.collect())
            .unwrap_or_default();
        let classes_lower: Vec<String> = classes.iter().map(|c| c.to_lowercase()).collect();
        let classes_joined = classes_lower.join(" ");

        // [7-9] Class features
        feat[7] = if classes_joined.contains("title") { 1.0 } else { 0.0 };
        feat[8] = if classes_joined.contains("entry") { 1.0 } else { 0.0 };
        feat[9] = if classes_joined.contains("info") { 1.0 } else { 0.0 };

        // [10] Has ID
        let id_val = tag.attributes().get(tl::Bytes::from("id")).flatten();
        feat[10] = if id_val.is_some() { 1.0 } else { 0.0 };

        // [11] Depth (approximate from tag structure)
        // We don't have easy access to depth in tl, so we use a heuristic
        let text = tag.inner_text(parser).trim().to_string();
        feat[11] = 0.4; // Average depth approximation for content nodes

        // [12-13] Sibling info (approximate)
        feat[12] = 0.0; // Can't easily determine in tl
        feat[13] = 0.3; // Approximate

        // [14] Child count (approximate from inner HTML complexity)
        let inner_text_len = text.len();
        feat[14] = 0.0; // Simplified — h1/h2/h3 typically have 0 children

        // [15] Text length
        feat[15] = (inner_text_len as f32 / 200.0).min(1.0);

        // [16] Text starts with "Komik"
        feat[16] = if text.to_lowercase().starts_with("komik") { 1.0 } else { 0.0 };

        // [17-18] Parent features (approximate from class context)
        feat[17] = if classes_joined.contains("info") || tag_name == "h1" { 1.0 } else { 0.0 };
        feat[18] = if classes_joined.contains("info") { 1.0 } else { 0.0 };

        // [19] Has itemprop attribute
        let has_itemprop = tag.attributes().get(tl::Bytes::from("itemprop")).flatten().is_some();
        feat[19] = if has_itemprop { 1.0 } else { 0.0 };

        // [20-22] Ancestor features (approximate — check class and id)
        let id_str = id_val
            .map(|v| v.try_as_utf8_str().unwrap_or("").to_lowercase())
            .unwrap_or_default();
        feat[20] = if id_str == "spe" || classes_joined.contains("spe") { 1.0 } else { 0.0 };
        feat[21] = if classes_joined.contains("infox") { 1.0 } else { 0.0 };
        feat[22] = if classes_joined.contains("infoanime") { 1.0 } else { 0.0 };

        // [23] Bold text ratio (simplified)
        feat[23] = 0.0; // Simplified — would need DOM walk for accuracy

        // [24] Link count (approximate)
        feat[24] = 0.0; // Simplified — title nodes typically have 0 links

        // [25] Has rel=tag
        let has_rel_tag = tag.attributes().get(tl::Bytes::from("rel")).flatten()
            .map(|v| v.try_as_utf8_str().unwrap_or("").contains("tag"))
            .unwrap_or(false);
        feat[25] = if has_rel_tag { 1.0 } else { 0.0 };

        // [26] Word count
        feat[26] = (text.split_whitespace().count() as f32 / 20.0).min(1.0);

        // [27] Has img child (simplified)
        feat[27] = 0.0; // Title nodes don't have img children

        // [28] Is first significant (simplified — h1 tags are typically first)
        feat[28] = if tag_name == "h1" { 1.0 } else { 0.0 };

        // [29] Font size indicator
        let font_map = [("h1", 1.0), ("h2", 0.8), ("h3", 0.6), ("h4", 0.5), ("h5", 0.4), ("h6", 0.3)];
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

        // Use ort::inputs! macro for type-safe input construction
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
