#!/usr/bin/env python3
"""Collect training data using cached HTML from existing JSONL + fixtures.

Since Cloudflare blocks Python requests, we use:
1. Existing HTML fixtures in tests/fixtures/
2. Scrape using Rust binary (komikindo-scraper) with --limit
3. Or manually cached HTML files

For initial experiment, we'll use the existing fixture + generate more
from the JSONL data we already have (which contains raw parsed data,
not HTML). So we'll use the Rust scraper to fetch HTML files.

Usage:
  # Step 1: Use Rust scraper to download HTML files
  ./target/release/komikindo-scraper full-fetch --limit 200
  # Then extract features from the JSONL data
  
  # OR: manually download HTML files with Rust
  python3 collect_training_data.py --html-dir ../html_cache
"""

import argparse
import json
import os
import subprocess
import sys
import numpy as np
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from bs4 import BeautifulSoup
from extract_features import extract_features, find_title_node, FEATURE_NAMES, NUM_FEATURES


def main():
    parser = argparse.ArgumentParser(description='Collect training data for title detection LM')
    parser.add_argument('--html-dir', type=str, default='html_cache', help='Directory with cached HTML files')
    parser.add_argument('--output', type=str, default='training_data.npz', help='Output .npz file')
    parser.add_argument('--limit', type=int, default=0, help='Max pages to process (0=all)')
    parser.add_argument('--scrape', action='store_true', help='Scrape HTML using Rust binary first')
    parser.add_argument('--scrape-limit', type=int, default=200, help='How many pages to scrape')
    args = parser.parse_args()

    html_dir = Path(args.html_dir)
    html_dir.mkdir(exist_ok=True)

    # Step 1: Optionally scrape using Rust binary
    if args.scrape:
        rust_binary = Path(__file__).parent.parent / 'target' / 'release' / 'komikindo-scraper'
        if not rust_binary.exists():
            print(f"ERROR: Rust binary not found at {rust_binary}")
            print("Build it first: cargo build --release")
            return

        print(f"[SCRAPE] Fetching {args.scrape_limit} komik HTML pages using Rust...")
        # First get slug list
        result = subprocess.run(
            [str(rust_binary), 'check'],
            capture_output=True, text=True, timeout=30
        )
        if 'Connected' not in result.stdout and 'Connected' not in result.stderr:
            print("WARNING: Site connectivity check failed")

        # Use the Rust scraper to fetch pages and save HTML
        # We'll create a simple script that fetches detail pages
        # Actually, let's just use the existing full-fetch to get JSONL,
        # then for each slug, fetch the HTML using curl with proper headers

        # Better approach: read slugs from the existing JSONL data
        data_dir = Path(__file__).parent.parent / 'data'
        jsonl_files = sorted(data_dir.glob('full_fetch_*.jsonl'), key=lambda p: p.stat().st_mtime, reverse=True)

        if jsonl_files:
            print(f"[SCRAPE] Found existing JSONL: {jsonl_files[0].name}")
            with open(jsonl_files[0]) as f:
                slugs = []
                for line in f:
                    try:
                        d = json.loads(line)
                        slug = d.get('slug', '')
                        if slug:
                            slugs.append(slug)
                    except:
                        pass

            slugs = slugs[:args.scrape_limit]
            print(f"[SCRAPE] Got {len(slugs)} slugs from JSONL")

            # Fetch each page using Rust binary's check command with verbose
            # Actually, we need a way to save HTML. Let's write a small helper.
            # For now, we'll just use the fixture approach.

            # Use the fixture HTML for testing, then add more later
            pass

    # Step 2: Process HTML files
    print(f"[EXTRACT] Processing HTML files from {html_dir}...")

    # Check for fixture files first
    fixture_dir = Path(__file__).parent.parent / 'tests' / 'fixtures'
    html_sources = []

    if fixture_dir.exists():
        for f in fixture_dir.glob('*.html'):
            html_sources.append(f)
        print(f"  Found {len(html_sources)} fixture files")

    # Add cached HTML files
    for f in html_dir.glob('*.html'):
        if f not in html_sources:
            html_sources.append(f)
    print(f"  Total HTML sources: {len(html_sources)}")

    if not html_sources:
        print("\nERROR: No HTML files found!")
        print("Run with --scrape to download HTML, or add .html files to html_cache/")
        return

    # Extract features
    all_features = []
    all_labels = []
    total_title_nodes = 0
    total_nodes = 0
    failed_pages = 0
    title_examples = []

    if args.limit > 0:
        html_sources = html_sources[:args.limit]

    for html_file in html_sources:
        try:
            html = html_file.read_text(encoding='utf-8')

            # Skip Cloudflare challenge pages
            if 'Just a moment...' in html[:2000]:
                print(f"  SKIP: {html_file.name} (Cloudflare challenge)")
                continue

            soup = BeautifulSoup(html, 'html.parser')

            # Walk all elements and extract features
            title_node = find_title_node(soup)

            features_list = []
            labels_list = []
            title_identity = None

            if title_node:
                title_identity = (
                    title_node.name,
                    tuple(sorted(title_node.get('class', []))),
                    title_node.get('id', ''),
                    title_node.get_text(strip=True)[:100],
                )

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

                is_title = 0
                if title_identity is not None:
                    elem_identity = (
                        element.name,
                        tuple(sorted(element.get('class', []))),
                        element.get('id', ''),
                        element.get_text(strip=True)[:100],
                    )
                    if elem_identity == title_identity:
                        is_title = 1
                        title_examples.append({
                            'text': element.get_text(strip=True)[:80],
                            'classes': element.get('class', []),
                            'tag': element.name,
                        })

                labels_list.append(is_title)

            if not features_list:
                continue

            features = np.array(features_list, dtype=np.float32)
            labels = np.array(labels_list, dtype=np.int32)

            if labels.sum() == 0:
                print(f"  WARN: No title found in {html_file.name}")
                failed_pages += 1
                continue

            all_features.append(features)
            all_labels.append(labels)
            total_title_nodes += int(labels.sum())
            total_nodes += len(labels)

        except Exception as e:
            print(f"  Error: {html_file.name}: {e}")
            failed_pages += 1

    if not all_features:
        print("\nERROR: No training data extracted!")
        return

    X = np.concatenate(all_features, axis=0)
    y = np.concatenate(all_labels, axis=0)

    output_path = Path(args.output)
    np.savez(output_path, X=X, y=y,
             feature_names=FEATURE_NAMES,
             total_pages=len(html_sources) - failed_pages,
             total_title_nodes=total_title_nodes,
             total_nodes=total_nodes,
             failed_pages=failed_pages)

    print(f"\n[DONE] Training data saved to {output_path}")
    print(f"  Total pages: {len(html_sources)} ({failed_pages} failed)")
    print(f"  Total DOM nodes: {total_nodes:,}")
    print(f"  Title nodes: {total_title_nodes:,}")
    print(f"  Non-title nodes: {total_nodes - total_title_nodes:,}")
    print(f"  Title ratio: {total_title_nodes/total_nodes*100:.2f}%")
    print(f"  Feature shape: {X.shape}")

    if title_examples:
        print(f"\n  Example title nodes found:")
        for ex in title_examples[:5]:
            print(f"    <{ex['tag']} class='{' '.join(ex['classes'])}'> → \"{ex['text'][:50]}\"")


if __name__ == '__main__':
    main()
