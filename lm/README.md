# LM Selector Engine — Title Detection Experiment

Goal: Train a small LM to detect which DOM node contains the komik title,
replacing hardcoded CSS selectors with a learned model.

## Architecture

```
HTML → DOM Walk → Feature Vector per Node → ONNX Inference → Title Node
```

## Feature Vector (per DOM node, 30 dims)

| # | Feature | Type | Description |
|---|---------|------|-------------|
| 0 | is_h1 | bool | Tag is <h1> |
| 1 | is_h2 | bool | Tag is <h2> |
| 2 | is_h3 | bool | Tag is <h3> |
| 3 | is_span | bool | Tag is <span> |
| 4 | is_div | bool | Tag is <div> |
| 5 | is_a | bool | Tag is <a> |
| 6 | is_td | bool | Tag is <td> |
| 7 | has_class_title | bool | Any class contains "title" |
| 8 | has_class_entry | bool | Any class contains "entry" |
| 9 | has_class_info | bool | Any class contains "info" |
| 10 | has_id | bool | Has non-empty id attribute |
| 11 | depth | float | Depth in DOM tree (0-20, /20) |
| 12 | sibling_index | float | Position among siblings (/20) |
| 13 | sibling_count | float | Number of siblings (/20) |
| 14 | child_count | float | Number of children (/50) |
| 15 | text_length | float | Text content length (/200) |
| 16 | text_starts_komik | bool | Text starts with "Komik" (case-insensitive) |
| 17 | parent_is_div | bool | Parent tag is <div> |
| 18 | parent_has_info | bool | Parent class contains "info" |
| 19 | has_itemprop | bool | Has itemprop attribute |
| 20 | is_inside_spe | bool | Ancestor has class "spe" |
| 21 | is_inside_infox | bool | Ancestor has class "infox" |
| 22 | is_inside_infoanime | bool | Ancestor has class "infoanime" |
| 23 | bold_text_ratio | float | Ratio of <b> text to total text |
| 24 | link_count | float | Number of <a> descendants (/10) |
| 25 | has_rel_tag | bool | Has rel="tag" attribute |
| 26 | text_word_count | float | Number of words in text (/20) |
| 27 | has_img_child | bool | Has <img> as direct child |
| 28 | is_first_significant | bool | First text-bearing node in parent |
| 29 | font_size_indicator | float | Heuristic: h1=1.0, h2=0.8, h3=0.6, else 0.0 |

## Model

- 3-layer MLP: 30 → 64 → 32 → 1 (sigmoid)
- ReLU activation
- Binary classification: is this node the title? (1=yes, 0=no)
- Loss: Binary Cross-Entropy
- Output: ONNX model file (~5KB)

## Training

- ~200 annotated HTML pages from komikindo
- sklearn MLPClassifier → exported to ONNX
- Training time: < 5 seconds
