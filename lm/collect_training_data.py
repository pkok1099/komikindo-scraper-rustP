#!/usr/bin/env python3
"""Collect training data using cached HTML from html_cache + fixtures.

Supports multiple fields: title, rating (more can be added).

Usage:
  python3 collect_training_data.py --field title
  python3 collect_training_data.py --field rating
  python3 collect_training_data.py --field all
"""

import argparse
import json
import os
import sys
import numpy as np
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from bs4 import BeautifulSoup
from extract_features import (
    extract_features, find_title_node, find_rating_node,
    FEATURE_NAMES, NUM_FEATURES
)


# Map field name → ground truth finder
FIELD_FINDERS = {
    'title': find_title_node,
    'rating': find_rating_node,
}


def collect_for_field(html_sources, field_name, finder_fn, limit=0):
    """Collect training data for a specific field."""
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

            # Skip Cloudflare challenge pages
            if 'Just a moment...' in html[:2000]:
                continue

            soup = BeautifulSoup(html, 'html.parser')

            # Find ground truth node
            gt_node = finder_fn(soup)

            gt_identity = None
            if gt_node:
                gt_identity = (
                    gt_node.name,
                    tuple(sorted(gt_node.get('class', []))),
                    gt_node.get('id', ''),
                    gt_node.get_text(strip=True)[:100],
                )

            # Walk all elements and extract features
            features_list = []
            labels_list = []

            for element in soup.find_all(True):
                if element.name in ['script', 'style', 'noscript', 'meta', 'link', 'head']:
                    continue

                depth = 0
                parent = element.parent
                while parent and parent.name:
                    depth += 1
                    parent = parent.parent

                feat = extract_features(element, depth)
                features_list.append(feat)

                is_positive = 0
                if gt_identity is not None:
                    elem_identity = (
                        element.name,
                        tuple(sorted(element.get('class', []))),
                        element.get('id', ''),
                        element.get_text(strip=True)[:100],
                    )
                    if elem_identity == gt_identity:
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
    parser.add_argument('--html-dir', type=str, default='html_cache', help='Directory with cached HTML files')
    parser.add_argument('--field', type=str, default='all', choices=['title', 'rating', 'all'],
                        help='Which field to collect data for')
    parser.add_argument('--limit', type=int, default=0, help='Max pages to process (0=all)')
    args = parser.parse_args()

    html_dir = Path(args.html_dir)

    # Collect HTML sources
    html_sources = []

    # Check fixtures
    fixture_dir = Path(__file__).parent.parent / 'tests' / 'fixtures'
    if fixture_dir.exists():
        for f in fixture_dir.glob('*.html'):
            html_sources.append(f)

    # Add cached HTML files
    if html_dir.exists():
        for f in html_dir.glob('*.html'):
            html_sources.append(f)

    print(f"Found {len(html_sources)} HTML source files")

    if not html_sources:
        print("\nERROR: No HTML files found!")
        return

    # Determine which fields to process
    fields = ['title', 'rating'] if args.field == 'all' else [args.field]

    for field_name in fields:
        print(f"\n{'='*60}")
        print(f"  Collecting training data for: {field_name}")
        print(f"{'='*60}")

        finder_fn = FIELD_FINDERS[field_name]
        result = collect_for_field(html_sources, field_name, finder_fn, args.limit)

        if result is None:
            print(f"\nERROR: No training data for {field_name}!")
            continue

        X, y = result['X'], result['y']

        # Save
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
        print(f"  Positive ratio: {result['total_positive']/result['total_nodes']*100:.2f}%")
        print(f"  Feature shape: {X.shape}")

        if result['examples']:
            print(f"\n  Example {field_name} nodes found:")
            for ex in result['examples'][:5]:
                print(f"    <{ex['tag']} class='{' '.join(ex['classes'])}'> → \"{ex['text'][:50]}\"")


if __name__ == '__main__':
    main()
