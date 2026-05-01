#!/usr/bin/env python3
"""Collect multi-label training data — 1 pass per page, all fields labeled.

Single unified model approach: every DOM node gets a multi-label vector:
  [is_title, is_rating, is_genre, is_synopsis, is_alt_title, is_author, is_status, is_similar, is_chapters]

This replaces the old per-field collection. The resulting training data
is used to train ONE model that outputs 9 probabilities simultaneously.

Usage:
  python3 collect_training_data.py                     # multi-label (default)
  python3 collect_training_data.py --mode multilabel   # same as above
  python3 collect_training_data.py --mode per-field    # legacy per-field mode
"""

import argparse
import sys
import numpy as np
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from bs4 import BeautifulSoup
from extract_features import (
    extract_features, find_title_node, find_rating_node,
    find_genre_nodes, find_synopsis_node,
    find_alt_title_node, find_author_node, find_status_node,
    find_similar_nodes, find_chapter_nodes,
    FEATURE_NAMES, NUM_FEATURES
)

# Field definitions — order matters, must match Rust FieldType enum
FIELD_NAMES = ['title', 'rating', 'genre', 'synopsis', 'alt_title', 'author', 'status', 'similar', 'chapters']
NUM_FIELDS = len(FIELD_NAMES)

# Map field name → ground truth finder
FIELD_FINDERS = {
    'title': find_title_node,
    'rating': find_rating_node,
    'genre': find_genre_nodes,
    'synopsis': find_synopsis_node,
    'alt_title': find_alt_title_node,
    'author': find_author_node,
    'status': find_status_node,
    'similar': find_similar_nodes,
    'chapters': find_chapter_nodes,
}

# Which fields return multiple nodes
MULTI_NODE_FIELDS = {'genre', 'similar', 'chapters'}


def _node_identity(element):
    """Create a hashable identity tuple for a DOM element."""
    return (
        element.name,
        tuple(sorted(element.get('class', []))),
        element.get('id', ''),
        element.get_text(strip=True)[:100],
    )


def collect_multilabel(html_sources, limit=0):
    """Collect multi-label training data: 1 pass per page, all fields at once.

    Returns X (N, 40) and Y (N, 9) where Y columns are:
      [is_title, is_rating, is_genre, is_synopsis, is_alt_title, is_author, is_status, is_similar, is_chapters]
    """
    if limit > 0:
        html_sources = html_sources[:limit]

    all_features = []
    all_labels = []
    field_totals = {f: 0 for f in FIELD_NAMES}
    total_nodes = 0
    failed_pages = 0
    examples = {f: [] for f in FIELD_NAMES}

    for html_file in html_sources:
        try:
            html = html_file.read_text(encoding='utf-8')

            # Skip Cloudflare challenge pages
            if 'Just a moment...' in html[:2000]:
                continue

            soup = BeautifulSoup(html, 'html.parser')

            # Find ground truth for ALL fields at once
            gt_identities = {}  # field_name → set of node identities
            any_found = False

            for field_name in FIELD_NAMES:
                finder_fn = FIELD_FINDERS[field_name]
                gt_result = finder_fn(soup)

                is_multi = field_name in MULTI_NODE_FIELDS

                if is_multi:
                    gt_nodes = gt_result if gt_result else []
                    identities = set()
                    for node in gt_nodes:
                        identities.add(_node_identity(node))
                else:
                    identities = set()
                    if gt_result is not None:
                        identities.add(_node_identity(gt_result))

                gt_identities[field_name] = identities
                if identities:
                    any_found = True

            # Require at least some field found
            if not any_found:
                failed_pages += 1
                continue

            # Walk all elements ONCE, extract features + multi-label
            # First collect all candidate elements to compute document position
            all_elements = [
                e for e in soup.find_all(True)
                if e.name not in ['script', 'style', 'noscript', 'meta', 'link', 'head']
            ]
            total_elements = len(all_elements)

            features_list = []
            labels_list = []

            for doc_idx, element in enumerate(all_elements):
                depth = 0
                parent = element.parent
                while parent and parent.name:
                    depth += 1
                    parent = parent.parent

                # Normalized document position: 0.0 = top, 1.0 = bottom
                doc_position = doc_idx / max(total_elements - 1, 1)

                feat = extract_features(element, depth, doc_position)
                features_list.append(feat)

                elem_id = _node_identity(element)
                label_vec = np.zeros(NUM_FIELDS, dtype=np.int32)

                for i, field_name in enumerate(FIELD_NAMES):
                    if elem_id in gt_identities[field_name]:
                        label_vec[i] = 1
                        field_totals[field_name] += 1
                        if len(examples[field_name]) < 5:
                            examples[field_name].append({
                                'text': element.get_text(strip=True)[:80],
                                'classes': element.get('class', []),
                                'tag': element.name,
                            })

                labels_list.append(label_vec)

            if not features_list:
                continue

            features = np.array(features_list, dtype=np.float32)
            labels = np.array(labels_list, dtype=np.int32)

            # Skip pages where no positive labels at all (shouldn't happen with any_found check)
            if labels.sum() == 0:
                continue

            all_features.append(features)
            all_labels.append(labels)
            total_nodes += len(labels)

        except Exception as e:
            print(f"  Error: {html_file.name}: {e}")
            failed_pages += 1

    if not all_features:
        return None

    X = np.concatenate(all_features, axis=0)
    Y = np.concatenate(all_labels, axis=0)

    return {
        'X': X, 'Y': Y,
        'field_totals': field_totals,
        'total_nodes': total_nodes,
        'failed_pages': failed_pages,
        'examples': examples,
    }


def collect_for_field_legacy(html_sources, field_name, finder_fn, limit=0):
    """Legacy per-field collection (backward compatible)."""
    is_multi = field_name in MULTI_NODE_FIELDS

    all_features = []
    all_labels = []
    total_positive = 0
    total_nodes = 0
    failed_pages = 0
    examples = []

    if limit > 0:
        html_sources = html_sources[:limit]

    for html_file in html_sources:
        try:
            html = html_file.read_text(encoding='utf-8')
            if 'Just a moment...' in html[:2000]:
                continue

            soup = BeautifulSoup(html, 'html.parser')
            gt_result = finder_fn(soup)

            if is_multi:
                gt_nodes = gt_result if gt_result else []
                gt_identities = set()
                for node in gt_nodes:
                    gt_identities.add(_node_identity(node))
            else:
                gt_identities = set()
                if gt_result is not None:
                    gt_identities.add(_node_identity(gt_result))

            # Collect all candidate elements for document position
            all_elements = [
                e for e in soup.find_all(True)
                if e.name not in ['script', 'style', 'noscript', 'meta', 'link', 'head']
            ]
            total_elements = len(all_elements)

            features_list = []
            labels_list = []

            for doc_idx, element in enumerate(all_elements):
                depth = 0
                parent = element.parent
                while parent and parent.name:
                    depth += 1
                    parent = parent.parent

                doc_position = doc_idx / max(total_elements - 1, 1)
                feat = extract_features(element, depth, doc_position)
                features_list.append(feat)

                is_positive = 0
                elem_id = _node_identity(element)
                if elem_id in gt_identities:
                    is_positive = 1
                    if len(examples) < 10:
                        examples.append({
                            'text': element.get_text(strip=True)[:80],
                            'classes': element.get('class', []),
                            'tag': element.name,
                        })

                labels_list.append(is_positive)

            if not features_list:
                continue

            features = np.array(features_list, dtype=np.float32)
            labels = np.array(labels_list, dtype=np.int32)

            if labels.sum() == 0:
                print(f"  WARN: No {field_name} found in {html_file.name}")
                failed_pages += 1
                continue

            all_features.append(features)
            all_labels.append(labels)
            total_positive += int(labels.sum())
            total_nodes += len(labels)

        except Exception as e:
            print(f"  Error: {html_file.name}: {e}")
            failed_pages += 1

    if not all_features:
        return None

    X = np.concatenate(all_features, axis=0)
    y = np.concatenate(all_labels, axis=0)

    return {
        'X': X, 'y': y,
        'total_positive': total_positive,
        'total_nodes': total_nodes,
        'failed_pages': failed_pages,
        'examples': examples,
    }


def main():
    parser = argparse.ArgumentParser(description='Collect training data for LM field detection')
    parser.add_argument('--html-dir', type=str, default='html_cache',
                        help='Directory with cached HTML files')
    parser.add_argument('--mode', type=str, default='multilabel',
                        choices=['multilabel', 'per-field'],
                        help='Collection mode: multilabel (unified) or per-field (legacy)')
    parser.add_argument('--field', type=str, default='all',
                        choices=FIELD_NAMES + ['all'],
                        help='Which field (per-field mode only)')
    parser.add_argument('--limit', type=int, default=0,
                        help='Max pages to process (0=all)')
    args = parser.parse_args()

    html_dir = Path(args.html_dir)

    # Collect HTML sources
    html_sources = []

    fixture_dir = Path(__file__).parent.parent / 'tests' / 'fixtures'
    if fixture_dir.exists():
        for f in fixture_dir.glob('*.html'):
            html_sources.append(f)

    if html_dir.exists():
        for f in html_dir.glob('*.html'):
            html_sources.append(f)

    print(f"Found {len(html_sources)} HTML source files")

    if not html_sources:
        print("\nERROR: No HTML files found!")
        return

    if args.mode == 'multilabel':
        # === MULTI-LABEL MODE ===
        print(f"\n{'='*60}")
        print(f"  Collecting MULTI-LABEL training data")
        print(f"  Fields: {FIELD_NAMES}")
        print(f"{'='*60}")

        result = collect_multilabel(html_sources, args.limit)

        if result is None:
            print("\nERROR: No training data collected!")
            return

        X, Y = result['X'], result['Y']

        # Save
        output_path = Path(__file__).parent / 'training_data_multilabel.npz'
        np.savez(output_path, X=X, Y=Y,
                 feature_names=FEATURE_NAMES,
                 field_names=FIELD_NAMES,
                 total_nodes=result['total_nodes'],
                 failed_pages=result['failed_pages'])

        print(f"\n[DONE] Multi-label training data saved to {output_path}")
        print(f"  Total DOM nodes: {result['total_nodes']:,}")
        print(f"  Feature shape: {X.shape}")
        print(f"  Label shape: {Y.shape}")
        print(f"\n  Per-field positive counts:")
        for field_name in FIELD_NAMES:
            col_idx = FIELD_NAMES.index(field_name)
            pos = int(Y[:, col_idx].sum())
            total = Y.shape[0]
            print(f"    {field_name:12s}: {pos:5d} positive ({pos/total*100:.2f}%)")

        for field_name in FIELD_NAMES:
            if result['examples'][field_name]:
                print(f"\n  Example {field_name} nodes:")
                for ex in result['examples'][field_name][:3]:
                    print(f"    <{ex['tag']} class='{' '.join(ex['classes'])}'> -> \"{ex['text'][:60]}\"")

    else:
        # === LEGACY PER-FIELD MODE ===
        fields = FIELD_NAMES if args.field == 'all' else [args.field]

        for field_name in fields:
            print(f"\n{'='*60}")
            print(f"  Collecting training data for: {field_name}")
            is_multi = field_name in MULTI_NODE_FIELDS
            if is_multi:
                print(f"  (multi-node field)")
            print(f"{'='*60}")

            finder_fn = FIELD_FINDERS[field_name]
            result = collect_for_field_legacy(html_sources, field_name, finder_fn, args.limit)

            if result is None:
                print(f"\nERROR: No training data for {field_name}!")
                continue

            X, y = result['X'], result['y']

            output_path = Path(__file__).parent / f'training_data_{field_name}.npz'
            np.savez(output_path, X=X, y=y,
                     feature_names=FEATURE_NAMES,
                     total_positive=result['total_positive'],
                     total_nodes=result['total_nodes'],
                     failed_pages=result['failed_pages'])

            print(f"\n[DONE] {field_name} training data saved to {output_path}")
            print(f"  Total DOM nodes: {result['total_nodes']:,}")
            print(f"  Positive ({field_name}) nodes: {result['total_positive']:,}")
            print(f"  Negative nodes: {result['total_nodes'] - result['total_positive']:,}")


if __name__ == '__main__':
    main()
