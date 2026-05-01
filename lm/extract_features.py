"""Feature extraction for DOM nodes — shared between collect and Rust integration.

Feature vector (40 dims):
  [0-8]   Tag one-hot: h1, h2, h3, span, div, a, td, i, meta
  [9-13]  Class contains: title, entry, info, rating, archive
  [14]    Has non-empty id
  [15-19] Structural: depth, sibling_index, sibling_count, child_count, text_length_log1p
  [20]    Text starts with "Komik"
  [21-22] Parent: is_div, has_info_class
  [23-24] Attribute: has_itemprop, itemprop_is_ratingValue
  [25-27] Inside ancestor: spe, infox, infoanime
  [28-30] Content: bold_text_ratio, link_count, has_rel_tag
  [31]    Font size indicator
  [32-34] Bold text label patterns: contains_status, contains_author, contains_alternative
  [35]    Class contains: desc/synopsis/entry-content
  [36]    Has href containing "-chapter-"
  [37]    Inside ancestor: mirip/bxcl
  [38]    Has class: lchx, series
  [39]    Text contains "Chapter"
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
    'depth', 'sibling_index', 'sibling_count', 'child_count', 'text_length_log1p',
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
    # NEW: Bold text label patterns (32-34)
    'bold_contains_status', 'bold_contains_author', 'bold_contains_alternative',
    # NEW: Class/content patterns (35)
    'has_class_desc_or_synopsis',
    # NEW: Chapter link pattern (36)
    'href_contains_chapter',
    # NEW: Ancestor: mirip/bxcl containers (37)
    'is_inside_mirip_or_bxcl',
    # NEW: Class: lchx/series (38)
    'has_class_lchx_or_series',
    # NEW: Text contains "Chapter" (39)
    'text_contains_chapter',
]

NUM_FEATURES = len(FEATURE_NAMES)  # 40


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


def _get_bold_text(element):
    """Get concatenated text from all <b>/<strong> children."""
    bold_texts = []
    for b in element.find_all(['b', 'strong']):
        bold_texts.append(b.get_text(strip=True).lower())
    return ' '.join(bold_texts)


def extract_features(element, depth=0):
    """Extract 40-dim feature vector from a DOM element."""
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
    # Log-transform text_length for better MLP convergence
    # Raw text_length has heavy-tail distribution (0 to ~10K+).
    # MLP is sensitive to feature scale, so log(1+x) compresses the range.
    # This is capped at 1.0 for very long texts (log1p(200) ≈ 5.3, normalized to ~0.95)
    text_len = len(element.get_text(strip=True))
    features[19] = min(np.log1p(text_len) / 6.0, 1.0)

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
    ancestor_ids = set()
    for anc in ancestors:
        ancestor_classes.update(get_classes(anc))
        anc_id = anc.get('id', '')
        if anc_id:
            ancestor_ids.add(anc_id.lower())
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

    # === NEW FEATURES (32-39) ===

    # Bold text label patterns (32-34) — for alt_title, author, status detection
    bold_text_joined = _get_bold_text(element)
    features[32] = 1.0 if 'status' in bold_text_joined else 0.0
    features[33] = 1.0 if any(kw in bold_text_joined for kw in ('pengarang', 'author')) else 0.0
    features[34] = 1.0 if any(kw in bold_text_joined for kw in ('alternative', 'alternatif')) else 0.0

    # Class: desc/synopsis/entry-content (35)
    features[35] = 1.0 if any(c in classes for c in ('desc', 'synopsis', 'entry-content')) else 0.0

    # href contains "-chapter-" (36)
    href = element.get('href', '')
    features[36] = 1.0 if '-chapter-' in href.lower() else 0.0

    # Ancestor: mirip/bxcl containers (37)
    features[37] = 1.0 if ('mirip' in ancestor_ids or
                           'mirip' in ancestor_classes or
                           'bxcl' in ancestor_classes or
                           'chapter_list' in ancestor_ids) else 0.0

    # Class: lchx/series (38)
    features[38] = 1.0 if any(c in classes for c in ('lchx', 'series')) else 0.0

    # Text contains "Chapter" (39)
    features[39] = 1.0 if 'chapter' in text.lower() else 0.0

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


def find_genre_nodes(soup):
    """Find all genre <a> nodes (multi-node field).

    Genre links: a[rel=tag] inside div.genre-info or div.spe.
    Returns a list of BeautifulSoup elements.
    """
    genres = []

    # Primary: a[rel=tag] inside genre containers
    genre_containers = ['div.genre-info', 'div.spe']
    for container_sel in genre_containers:
        container = soup.select_one(container_sel)
        if container:
            links = container.find_all('a', rel='tag')
            if links:
                genres.extend(links)
                break

    # Fallback: any a[rel=tag] on the page
    if not genres:
        genres = soup.find_all('a', rel='tag')

    return genres


def find_synopsis_node(soup):
    """Find the synopsis/description node (single-node field).

    Synopsis: div.desc inside section.whites
    """
    desc = soup.select_one('div.desc')
    if desc and desc.get_text(strip=True):
        return desc

    # Fallback: any div with class containing "desc" or "synopsis"
    for div in soup.find_all('div'):
        classes = get_classes(div)
        if any('desc' in c or 'synopsis' in c for c in classes):
            text = div.get_text(strip=True)
            if len(text) > 50:  # Synopsis should be substantial text
                return div

    return None


def find_alt_title_node(soup):
    """Find the alternative title <span> node (single-node field).

    Alt title is in: div.spe > span containing <b>Alternative/Alternatif</b>
    The span itself is the target node.
    """
    spe = soup.select_one('div.spe')
    if spe:
        for span in spe.find_all('span'):
            bold = span.find(['b', 'strong'])
            if bold:
                bold_text = bold.get_text(strip=True).lower().rstrip(':')
                if 'alternative' in bold_text or 'alternatif' in bold_text:
                    return span
    return None


def find_author_node(soup):
    """Find the author <span> node (single-node field).

    Author is in: div.spe > span containing <b>Pengarang/Author</b>
    """
    spe = soup.select_one('div.spe')
    if spe:
        for span in spe.find_all('span'):
            bold = span.find(['b', 'strong'])
            if bold:
                bold_text = bold.get_text(strip=True).lower().rstrip(':')
                if 'pengarang' in bold_text or 'author' in bold_text:
                    return span
    return None


def find_status_node(soup):
    """Find the status <span> node (single-node field).

    Status is in: div.spe > span containing <b>Status</b>
    """
    spe = soup.select_one('div.spe')
    if spe:
        for span in spe.find_all('span'):
            bold = span.find(['b', 'strong'])
            if bold:
                bold_text = bold.get_text(strip=True).lower().rstrip(':')
                if 'status' in bold_text:
                    return span
    return None


def find_similar_nodes(soup):
    """Find similar/recommended komik <a> nodes (multi-node field).

    Similar links: div#mirip > li > a.series[href*="/komik/"]
    Returns a list of BeautifulSoup elements.
    """
    similar = []
    mirip = soup.select_one('div#mirip')
    if mirip:
        # Find all a.series links with /komik/ href inside mirip
        for a in mirip.find_all('a', class_='series'):
            href = a.get('href', '')
            if '/komik/' in href:
                similar.append(a)
    return similar


def find_chapter_nodes(soup):
    """Find chapter <a> nodes (multi-node field).

    Chapter links: div.bxcl a[href*='-chapter-'] or span.lchx a
    Returns a list of BeautifulSoup elements.
    """
    chapters = []
    container_selectors = ['div.bxcl', 'div#chapter_list']

    for container_sel in container_selectors:
        container = soup.select_one(container_sel)
        if container:
            # Primary: a[href*='-chapter-']
            for a in container.find_all('a', href=True):
                if '-chapter-' in a.get('href', '').lower():
                    chapters.append(a)
            if chapters:
                return chapters

    # Fallback: any a[href*='-chapter-']
    for a in soup.find_all('a', href=True):
        if '-chapter-' in a.get('href', '').lower():
            chapters.append(a)

    return chapters
