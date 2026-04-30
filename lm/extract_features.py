"""Feature extraction for DOM nodes — shared between collect and Rust integration."""

import numpy as np
from bs4 import BeautifulSoup

# Tag vocabulary (top 7 most common in komikindo detail pages)
TAG_VOCAB = ['h1', 'h2', 'h3', 'span', 'div', 'a', 'td']

FEATURE_NAMES = [
    'is_h1', 'is_h2', 'is_h3', 'is_span', 'is_div', 'is_a', 'is_td',
    'has_class_title', 'has_class_entry', 'has_class_info',
    'has_id',
    'depth', 'sibling_index', 'sibling_count', 'child_count', 'text_length',
    'text_starts_komik',
    'parent_is_div', 'parent_has_info',
    'has_itemprop',
    'is_inside_spe', 'is_inside_infox', 'is_inside_infoanime',
    'bold_text_ratio', 'link_count', 'has_rel_tag',
    'text_word_count', 'has_img_child',
    'is_first_significant', 'font_size_indicator',
]

NUM_FEATURES = len(FEATURE_NAMES)  # 30


def get_ancestors(element):
    ancestors = []
    parent = element.parent
    while parent is not None and parent.name is not None:
        ancestors.append(parent)
        parent = parent.parent
    return ancestors


def get_classes(element):
    classes = element.get('class', [])
    if isinstance(classes, str):
        classes = classes.split()
    return set(c.lower() for c in classes)


def is_first_significant(element):
    parent = element.parent
    if parent is None:
        return False
    for sibling in parent.children:
        if hasattr(sibling, 'name') and sibling.name is not None:
            text = sibling.get_text(strip=True)
            if len(text) > 2:
                return sibling is element
    return False


def extract_features(element, depth=0):
    """Extract 30-dim feature vector from a DOM element."""
    features = np.zeros(NUM_FEATURES, dtype=np.float32)

    tag = element.name.lower() if element.name else ''

    # Tag one-hot (0-6)
    for i, vocab_tag in enumerate(TAG_VOCAB):
        if tag == vocab_tag:
            features[i] = 1.0
            break

    # Class-based features (7-9)
    classes = get_classes(element)
    features[7] = 1.0 if any('title' in c for c in classes) else 0.0
    features[8] = 1.0 if any('entry' in c for c in classes) else 0.0
    features[9] = 1.0 if any('info' in c for c in classes) else 0.0

    # Has ID (10)
    features[10] = 1.0 if element.get('id') else 0.0

    # Structural features (11-15)
    features[11] = min(depth / 20.0, 1.0)

    parent = element.parent
    siblings = [s for s in parent.children if hasattr(s, 'name') and s.name is not None] if parent else []
    features[12] = min(siblings.index(element) / 20.0, 1.0) if element in siblings else 0.0
    features[13] = min(len(siblings) / 20.0, 1.0)
    children = [c for c in element.children if hasattr(c, 'name') and c.name is not None]
    features[14] = min(len(children) / 50.0, 1.0)
    features[15] = min(len(element.get_text(strip=True)) / 200.0, 1.0)

    # Text features (16)
    text = element.get_text(strip=True)
    features[16] = 1.0 if text.lower().startswith('komik') else 0.0

    # Parent features (17-18)
    if parent and parent.name:
        features[17] = 1.0 if parent.name.lower() == 'div' else 0.0
        parent_classes = get_classes(parent)
        features[18] = 1.0 if any('info' in c for c in parent_classes) else 0.0

    # Attribute features (19)
    features[19] = 1.0 if element.get('itemprop') else 0.0

    # Ancestor features (20-22)
    ancestors = get_ancestors(element)
    ancestor_classes = set()
    for anc in ancestors:
        ancestor_classes.update(get_classes(anc))
    features[20] = 1.0 if 'spe' in ancestor_classes else 0.0
    features[21] = 1.0 if 'infox' in ancestor_classes else 0.0
    features[22] = 1.0 if 'infoanime' in ancestor_classes else 0.0

    # Bold text ratio (23)
    bold_len = sum(len(b.get_text(strip=True)) for b in element.find_all(['b', 'strong']))
    total_len = len(text)
    features[23] = (bold_len / total_len) if total_len > 0 else 0.0

    # Link count (24)
    features[24] = min(len(element.find_all('a')) / 10.0, 1.0)

    # Has rel=tag (25)
    features[25] = 1.0 if element.get('rel') and 'tag' in element.get('rel', []) else 0.0

    # Word count (26)
    features[26] = min(len(text.split()) / 20.0, 1.0)

    # Has img child (27)
    features[27] = 1.0 if element.find('img', recursive=False) else 0.0

    # Is first significant (28)
    features[28] = 1.0 if is_first_significant(element) else 0.0

    # Font size indicator (29)
    font_map = {'h1': 1.0, 'h2': 0.8, 'h3': 0.6, 'h4': 0.5, 'h5': 0.4, 'h6': 0.3}
    features[29] = font_map.get(tag, 0.0)

    return features


def find_title_node(soup):
    """Find the title node using current hardcoded selectors (ground truth)."""
    selectors = ['h1.titless', 'h1.entry-title']

    for sel in selectors:
        found = soup.select_one(sel)
        if found and found.get_text(strip=True):
            return found

    infox = soup.select_one('div.infox')
    if infox:
        h1 = infox.find('h1')
        if h1 and h1.get_text(strip=True):
            return h1

    h1 = soup.find('h1')
    if h1 and h1.get_text(strip=True):
        return h1

    return None
