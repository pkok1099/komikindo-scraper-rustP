#!/usr/bin/env python3
"""Fast JSONL → PostgreSQL importer using execute_values."""
import json, sys, time

DB_URL = "postgresql://postgres.yjsrhbnnaxstwwszcobb:Izal10909143@aws-1-ap-southeast-1.pooler.supabase.com:5432/postgres"
JSONL_PATH = sys.argv[1] if len(sys.argv) > 1 else "data/full_fetch_20260429_072555.jsonl"
SKIP = int(sys.argv[2]) if len(sys.argv) > 2 else 0
BATCH_SIZE = 200  # komik per batch

def main():
    import psycopg2, psycopg2.extras

    print(f"Connecting to {DB_URL.split('@')[1]}...")
    conn = psycopg2.connect(DB_URL, connect_timeout=15)
    conn.autocommit = True  # Critical: autocommit for speed with Supabase
    cur = conn.cursor()

    # Ensure UNIQUE constraint
    print("Ensuring schema...")
    cur.execute("""DO $$ BEGIN
        IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'chapters_komik_id_chapter_number_key') THEN
            DELETE FROM chapters a USING chapters b WHERE a.id < b.id AND a.komik_id = b.komik_id AND a.chapter_number = b.chapter_number;
            ALTER TABLE chapters ADD CONSTRAINT chapters_komik_id_chapter_number_key UNIQUE (komik_id, chapter_number);
        END IF;
    END; $$""")

    # Ensure chapter_url column
    cur.execute("ALTER TABLE chapters ADD COLUMN IF NOT EXISTS chapter_url TEXT")
    print("Schema OK")

    # Read JSONL
    print(f"Reading {JSONL_PATH}...")
    with open(JSONL_PATH) as f:
        lines = [l.strip() for l in f if l.strip()]

    details = []
    for line in lines:
        try:
            d = json.loads(line)
            if '_status' not in d and 'slug' in d:
                details.append(d)
        except:
            pass

    if SKIP > 0:
        details = details[SKIP:]
        print(f"Skipped first {SKIP} entries (resuming)")

    total = len(details)
    if total == 0:
        print("Nothing to import!")
        return
    print(f"Loaded {total} komik entries")

    new_count = 0
    updated_count = 0
    failed = 0
    start = time.time()

    for batch_start in range(0, total, BATCH_SIZE):
        batch = details[batch_start:batch_start + BATCH_SIZE]

        try:
            # 1. Upsert all komik in batch
            komik_ids = []
            for d in batch:
                cur.execute("""
                    INSERT INTO komik (slug, judul, thumb_domain_id, thumb_path, tipe,
                        status_id, author, artist, alternative_title, sinopsis,
                        rating, latest_chapter_number, chapter_count)
                    VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s)
                    ON CONFLICT (slug) DO UPDATE SET
                        judul=EXCLUDED.judul, thumb_domain_id=EXCLUDED.thumb_domain_id,
                        thumb_path=EXCLUDED.thumb_path, tipe=EXCLUDED.tipe,
                        status_id=EXCLUDED.status_id, author=EXCLUDED.author,
                        artist=EXCLUDED.artist, alternative_title=EXCLUDED.alternative_title,
                        sinopsis=EXCLUDED.sinopsis, rating=EXCLUDED.rating,
                        latest_chapter_number=EXCLUDED.latest_chapter_number,
                        chapter_count=EXCLUDED.chapter_count
                    RETURNING id,(xmax=0)
                """, (
                    d['slug'], d.get('judul') or d['slug'],
                    d.get('thumb_domain_id'), d.get('thumb_path'), d.get('tipe'),
                    d.get('status_id'), d.get('author'), d.get('artist'),
                    d.get('alternative_title'), d.get('sinopsis'),
                    d.get('rating'), d.get('latest_chapter_number'),
                    len(d.get('chapters', []))
                ))
                row = cur.fetchone()
                komik_ids.append((row[0], d['slug'], row[1]))

            for _, slug, is_new in komik_ids:
                if is_new:
                    new_count += 1
                else:
                    updated_count += 1

            # 2. Bulk insert ALL chapters using execute_values
            # Deduplicate by (komik_id, chapter_number) to avoid ON CONFLICT error
            chapter_args = []
            seen_ch = set()
            for kid, slug, _ in komik_ids:
                d = next(x for x in batch if x['slug'] == slug)
                for ch in d.get('chapters', []):
                    num = ch.get('number')
                    if (kid, num) not in seen_ch:
                        seen_ch.add((kid, num))
                        chapter_args.append((kid, num, ch.get('url')))

            if chapter_args:
                psycopg2.extras.execute_values(cur,
                    """INSERT INTO chapters (komik_id,chapter_number,chapter_url) VALUES %s
                       ON CONFLICT (komik_id,chapter_number) DO UPDATE SET chapter_url=EXCLUDED.chapter_url,updated_at=now()""",
                    chapter_args, page_size=1000)

            # 3. Sync genres - delete old + bulk insert new
            kid_list = [kid for kid, _, _ in komik_ids]
            cur.execute("DELETE FROM komik_genres WHERE komik_id = ANY(%s)", (kid_list,))

            genre_args = []
            seen_g = set()
            for kid, slug, _ in komik_ids:
                d = next(x for x in batch if x['slug'] == slug)
                for gid in d.get('genre_ids', []):
                    if (kid, gid) not in seen_g:
                        seen_g.add((kid, gid))
                        genre_args.append((kid, gid))

            if genre_args:
                psycopg2.extras.execute_values(cur,
                    "INSERT INTO komik_genres (komik_id,genre_id) VALUES %s ON CONFLICT DO NOTHING",
                    genre_args, page_size=1000)

            done = new_count + updated_count
            elapsed = time.time() - start
            rate = done / elapsed * 60 if elapsed > 0 else 0
            eta = (total - batch_start - len(batch)) / rate / 60 if rate > 0 else 0
            print(f"  [{done}/{total}] batch {batch_start//BATCH_SIZE+1}: +{len(batch)} komik | "
                  f"{rate:.0} k/min | ETA:{eta:.1f}m | new={new_count} upd={updated_count} fail={failed}",
                  flush=True)

        except Exception as e:
            failed += len(batch)
            print(f"  [ERR] batch {batch_start//BATCH_SIZE+1}: {e}", flush=True)

    elapsed = time.time() - start
    done = new_count + updated_count
    print(f"\n{'='*50}")
    print(f"IMPORT COMPLETE: {done} komik ({new_count} new, {updated_count} upd, {failed} fail)")
    print(f"Time: {elapsed:.1}s ({elapsed/60:.1f}min) | {done/elapsed*60:.0f} komik/min")
    print(f"{'='*50}")

    cur.execute("SELECT COUNT(*) FROM komik")
    print(f"DB komik: {cur.fetchone()[0]}")
    cur.execute("SELECT COUNT(*) FROM chapters")
    print(f"DB chapters: {cur.fetchone()[0]}")

    conn.close()

if __name__ == "__main__":
    main()
