"""Feature extraction for DOM nodes — shared between collect and Rust integration.

Feature vector (32 dims):
  [0-8]   Tag one-hot: h1, h2, h3, span, div, a, td, i, meta
  [9-13]  Class contains: title, entry, info, rating, archive
  [14]    Has non-empty id
  [15-19] Structural: depth, sibling_index, sibling_count, child_count, text_length
  [20]    Text starts with "Komik"
  [21-22] Parent: is_div, has_info_class
  [23-24] Attribute: has_itemprop, itemprop_is_ratingValue
  [25-27] Inside ancestor: spe, infox, infoanime
  [28]    Bold text ratio
  [29-30] Link count, has rel=tag
  [31]    Font size indicator
"""

import numpy as np
from bs4 import BeautifulSoup

# Tag vocabulary (top 9 most common in komikindo detail pages, includes <i> for rating)
TAG_VOCAB = ['h1', 'h2', 'h3', 'span', 'div', 'a', 'td', 'i', 'meta']

FEATURE_NAMES = [
    # Tag one-hot (0-8)
    'is_h1', 'is_h2', 'is_h3', 'is_span', 'is_div', 'is_a', 'is_td', 'is_i', 'is_meta',
    # Class features (9-13)
    'has_class_title', 'has_class_entry', 'has_class_info', 'has_class_rating', 'has_class_archive',
    # ID (14)
    'has_id',
    # Structural (15-19)
    'depth', 'sibling_index', 'sibling_count', 'child_count', 'text_length',
    # Text features (20)
    'text_starts_komik',
    # Parent features (21-22)
    'parent_is_div', 'parent_has_info',
    # Attribute features (23-24)
    'has_itemprop', 'itemprop_is_ratingValue',
    # Ancestor features (25-27)
    'is_inside_spe', 'is_inside_infox', 'is_inside_infoanime',
    # Content features (28-30)
    'bold_text_ratio', 'link_count', 'has_rel_tag',
    # Semantic features (31)
    'font_size_indicator',
]

NUM_FEATURES = len(FEATURE_NAMES)  # 32


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
    """Extract 32-dim feature vector from a DOM element."""
    features = np.zeros(NUM_FEATURES, dtype=np.float32)

    tag = element.name.lower() if element.name else ''

    # Tag one-hot (0-8)
    for i, vocab_tag in enumerate(TAG_VOCAB):
        if tag == vocab_tag:
            features[i] = 1.0
            break

    # Class-based features (9-13)
    classes = get_classes(element)
    features[9] = 1.0 if any('title' in c for c in classes) else 0.0
    features[10] = 1.0 if any('entry' in c for c in classes) else 0.0
    features[11] = 1.0 if any('info' in c for c in classes) else 0.0
    features[12] = 1.0 if any('rating' in c for c in classes) else 0.0
    features[13] = 1.0 if any('archive' in c for c in classes) else 0.0

    # Has ID (14)
    features[14] = 1.0 if element.get('id') else 0.0

    # Structural features (15-19)
    features[15] = min(depth / 20.0, 1.0)

    parent = element.parent
    siblings = [s for s in parent.children if hasattr(s, 'name') and s.name is not None] if parent else []
    features[16] = min(siblings.index(element) / 20.0, 1.0) if element in siblings else 0.0
    features[17] = min(len(siblings) / 20.0, 1.0)
    children = [c for c in element.children if hasattr(c, 'name') and c.name is not None]
    features[18] = min(len(children) / 50.0, 1.0)
    features[19] = min(len(element.get_text(strip=True)) / 200.0, 1.0)

    # Text features (20)
    text = element.get_text(strip=True)
    features[20] = 1.0 if text.lower().startswith('komik') else 0.0

    # Parent features (21-22)
    if parent and parent.name:
        features[21] = 1.0 if parent.name.lower() == 'div' else 0.0
        parent_classes = get_classes(parent)
        features[22] = 1.0 if any('info' in c for c in parent_classes) else 0.0

    # Attribute features (23-24)
    itemprop = element.get('itemprop')
    features[23] = 1.0 if itemprop else 0.0
    features[24] = 1.0 if itemprop and itemprop == 'ratingValue' else 0.0

    # Ancestor features (25-27)
    ancestors = get_ancestors(element)
    ancestor_classes = set()
    for anc in ancestors:
        ancestor_classes.update(get_classes(anc))
    features[25] = 1.0 if 'spe' in ancestor_classes else 0.0
    features[26] = 1.0 if 'infox' in ancestor_classes else 0.0
    features[27] = 1.0 if 'infoanime' in ancestor_classes else 0.0

    # Bold text ratio (28)
    bold_len = sum(len(b.get_text(strip=True)) for b in element.find_all(['b', 'strong']))
    total_len = len(text)
    features[28] = (bold_len / total_len) if total_len > 0 else 0.0

    # Link count (29)
    features[29] = min(len(element.find_all('a')) / 10.0, 1.0)

    # Has rel=tag (30)
    features[30] = 1.0 if element.get('rel') and 'tag' in element.get('rel', []) else 0.0

    # Font size indicator (31)
    font_map = {'h1': 1.0, 'h2': 0.8, 'h3': 0.6, 'h4': 0.5, 'h5': 0.4, 'h6': 0.3}
    features[31] = font_map.get(tag, 0.0)

    return features


# ============================================================
# Ground truth selectors for each field
# ============================================================

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


def find_rating_node(soup):
    """Find the rating node using current hardcoded selectors (ground truth).

    Rating is in: div.archiveanime-rating > i[itemprop="ratingValue"]
    Also try: div.infoanime-rating, div.rating, span.skor
    """
    # Try itemprop="ratingValue" inside rating containers
    rating_containers = ['div.archiveanime-rating', 'div.infoanime-rating', 'div.rating']
    for container_sel in rating_containers:
        container = soup.select_one(container_sel)
        if container:
            i_tag = container.find('i', attrs={'itemprop': 'ratingValue'})
            if i_tag and i_tag.get_text(strip=True):
                return i_tag

    # Fallback: any i[itemprop="ratingValue"]
    i_tag = soup.find('i', attrs={'itemprop': 'ratingValue'})
    if i_tag and i_tag.get_text(strip=True):
        return i_tag

    # Fallback: span.skor
    span = soup.select_one('span.skor')
    if span and span.get_text(strip=True):
        return span

    return None
