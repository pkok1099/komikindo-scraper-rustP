#!/usr/bin/env python3
"""Download HTML files using the Rust scraper binary (which bypasses Cloudflare).

Uses the Rust scraper's check command to verify connectivity,
then uses curl with proper headers to download individual pages.

Actually, better approach: use the Rust scraper as a proxy to save HTML.
We create a simple shell script that calls the Rust scraper for each slug.

Simplest approach: use the Rust full-fetch to populate JSONL, then
for each slug in the JSONL, use curl to download the HTML.
"""

import json
import subprocess
import sys
import time
from pathlib import Path


def download_html_with_curl(slug: str, output_dir: Path, timeout: int = 15) -> bool:
    """Download a single HTML page using curl with proper headers."""
    url = f'https://komikindo.ch/komik/{slug}/'
    output_file = output_dir / f'{slug}.html'

    if output_file.exists():
        return True  # Already cached

    result = subprocess.run(
        ['curl', '-sS', '-L', '--max-time', str(timeout),
         '-H', 'User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36',
         '-H', 'Accept: text/html,application/xhtml+xml',
         '-H', 'Accept-Language: id-ID,id;q=0.9,en-US;q=0.8',
         '-H', 'Accept-Encoding: gzip, deflate',
         '--compressed',
         '-o', str(output_file),
         '-w', '%{http_code}',
         url],
        capture_output=True, text=True, timeout=timeout + 5
    )

    http_code = result.stdout.strip() if result.stdout else '000'

    if http_code == '200' and output_file.exists():
        # Check it's not a Cloudflare challenge
        content = output_file.read_text(encoding='utf-8', errors='ignore')[:2000]
        if 'Just a moment...' in content or 'cf-challenge' in content:
            output_file.unlink()
            return False
        return True
    else:
        if output_file.exists():
            output_file.unlink()
        return False


def main():
    html_dir = Path(__file__).parent / 'html_cache'
    html_dir.mkdir(exist_ok=True)

    # Get slugs from existing JSONL
    data_dir = Path(__file__).parent.parent / 'data'
    jsonl_files = sorted(data_dir.glob('full_fetch_*.jsonl'), key=lambda p: p.stat().st_mtime, reverse=True)

    if not jsonl_files:
        print("ERROR: No JSONL files found. Run full-fetch first.")
        return

    print(f"Using JSONL: {jsonl_files[0].name}")

    slugs = []
    with open(jsonl_files[0]) as f:
        for line in f:
            try:
                d = json.loads(line)
                slug = d.get('slug', '')
                if slug:
                    slugs.append(slug)
            except:
                pass

    print(f"Found {len(slugs)} slugs")

    # Download HTML for each slug
    success = 0
    failed = 0
    skipped = 0

    for i, slug in enumerate(slugs[:200]):  # Limit to 200
        output_file = html_dir / f'{slug}.html'
        if output_file.exists():
            skipped += 1
            continue

        ok = download_html_with_curl(slug, html_dir)
        if ok:
            success += 1
        else:
            failed += 1

        if (i + 1) % 20 == 0:
            print(f"  [{i+1}/{min(len(slugs), 200)}] OK: {success} FAIL: {failed} SKIP: {skipped}")

        time.sleep(0.05)  # Small delay to be polite

    print(f"\n[DONE] Downloaded: {success}, Failed: {failed}, Skipped: {skipped}")
    print(f"HTML files in: {html_dir}")


if __name__ == '__main__':
    main()
