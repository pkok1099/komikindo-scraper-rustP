# Database Schema & Operations

Dokumen ini menjelaskan schema database, operasi yang tersedia, dan troubleshooting untuk Supabase PostgreSQL backend.

---

## Connection

### Connection String Format

```
postgresql://postgres.PROJECT_REF:PASSWORD@aws-1-ap-southeast-1.pooler.supabase.com:6543/postgres
```

**Important:** Kode otomatis redirect PgBouncer port 6543 → direct port 5432 karena sqlx membutuhkan prepared statements yang tidak didukung PgBouncer.

### Connection Pool Settings

| Setting | Value | Reason |
|---------|-------|--------|
| max_connections | 50 | Balance antara throughput dan Supabase limits |
| acquire_timeout | 30s | Long enough untuk pool contention |
| idle_timeout | 300s (5min) | Close idle connections |
| max_lifetime | 1800s (30min) | Recycle connections periodically |
| ssl_mode | Prefer | Try TLS, fallback to plaintext |

---

## Schema

### Table: `komik`

```sql
CREATE TABLE komik (
    id SERIAL PRIMARY KEY,
    slug TEXT UNIQUE NOT NULL,        -- e.g., "155895-nano-machine"
    judul TEXT NOT NULL,              -- Judul komik
    thumb_domain_id SMALLINT NULL,    -- CDN domain reference (1-4)
    thumb_path TEXT NULL,             -- Thumbnail path (normalized)
    tipe TEXT NULL,                   -- "Manga", "Manhwa", "Manhua"
    status_id SMALLINT NULL,          -- 1=Berjalan, 2=Tamat
    author TEXT NULL,                 -- Pengarang
    artist TEXT NULL,                 -- Ilustrator
    alternative_title TEXT NULL,      -- Judul alternatif
    sinopsis TEXT NULL,               -- Sinopsis lengkap
    rating DOUBLE PRECISION NULL,     -- Rating (0.0-10.0)
    latest_chapter_number NUMERIC NULL, -- Chapter terbaru
    chapter_count INTEGER NULL,       -- Jumlah total chapters
    created_at TIMESTAMPTZ DEFAULT now(),
    updated_at TIMESTAMPTZ DEFAULT now()
);
```

**Indexes:**
- `komik_slug_unique` — UNIQUE constraint pada slug (serves as index)

**Triggers:**
- `trg_komik_updated_at` — Auto-update `updated_at` on UPDATE

### Table: `chapters`

```sql
CREATE TABLE chapters (
    id BIGSERIAL PRIMARY KEY,
    komik_id INTEGER NOT NULL REFERENCES komik(id) ON DELETE CASCADE,
    chapter_number NUMERIC NOT NULL,    -- e.g., 309.0, 124.5
    chapter_url TEXT NULL,              -- Full URL dari detail page
    cdn_domain_id SMALLINT NULL,        -- CDN domain (unused in Method 2)
    cdn_path_prefix TEXT NULL,          -- CDN path prefix (unused in Method 2)
    image_filenames TEXT NULL,          -- Image list (unused in Method 2)
    image_ext_id SMALLINT NULL,         -- Image extension ID (unused in Method 2)
    total_images SMALLINT DEFAULT 0,    -- Image count (unused in Method 2)
    created_at TIMESTAMPTZ DEFAULT now(),
    updated_at TIMESTAMPTZ DEFAULT now(),
    UNIQUE (komik_id, chapter_number)   -- Prevents duplicate chapters
);
```

**Indexes:**
- `chapters_komik_id_chapter_number_key` — UNIQUE constraint (also serves as index)
- `idx_chapters_komik_id` — Index on komik_id
- `idx_chapters_num` — Index on chapter_number
- `idx_chapters_komik_num` — Composite index on (komik_id, chapter_number DESC)

**Triggers:**
- `trg_chapters_updated_at` — Auto-update `updated_at` on UPDATE

### Table: `komik_genres`

```sql
CREATE TABLE komik_genres (
    komik_id INTEGER NOT NULL REFERENCES komik(id) ON DELETE CASCADE,
    genre_id SMALLINT NOT NULL,        -- 1-82 (hardcoded genre map)
    PRIMARY KEY (komik_id, genre_id)
);
```

### Table: `scrape_log`

```sql
CREATE TABLE scrape_log (
    id BIGSERIAL PRIMARY KEY,
    operation TEXT NOT NULL,           -- "full_fetch", "update", "upload_db"
    status TEXT NOT NULL,              -- "success", "partial", "failed"
    total_processed INTEGER NOT NULL DEFAULT 0,
    total_new INTEGER NOT NULL DEFAULT 0,
    total_updated INTEGER NOT NULL DEFAULT 0,
    total_failed INTEGER NOT NULL DEFAULT 0,
    details TEXT NULL,                 -- Additional info (JSON string)
    created_at TIMESTAMPTZ DEFAULT now()
);
```

---

## CDN Domain Map

| ID | Domain | Base URL |
|----|--------|----------|
| 1 | komikindo.ch | https://komikindo.ch |
| 2 | imageainewgeneration.lol | https://imageainewgeneration.lol |
| 3 | himmga.lat | https://himmga.lat |
| 4 | gaimgame.pics | https://gaimgame.pics |

---

## Genre Map

82 genre hardcoded dari komikindo.ch/daftar-manga/:

| ID | Genre | ID | Genre | ID | Genre |
|----|-------|----|-------|----|-------|
| 1 | Action | 29 | Josei | 57 | Sci-Fi |
| 2 | Adult | 30 | Life | 58 | Seinen |
| 3 | Adventure | 31 | Loli | 59 | Sexual Violence |
| 4 | Aliens | 32 | Mafia | 60 | Shota |
| 5 | Animals | 33 | Magic | 61 | Shoujo |
| 6 | Arts | 34 | Magical Girls | 62 | Shoujo Ai |
| 7 | Boys' Love | 35 | Martial | 63 | Shounen |
| 8 | Comedy | 36 | Martial Arts | 64 | Shounen Ai |
| 9 | Cooking | 37 | Mature | 65 | Slice of Life |
| 10 | Crime | 38 | Mecha | 66 | Smut |
| 11 | Crossdressing | 39 | Medical | 67 | Sports |
| 12 | Delinquents | 40 | Military | 68 | Superhero |
| 13 | Demons | 41 | Monster Girls | 69 | Supernatural |
| 14 | Drama | 42 | Monsters | 70 | Survival |
| 15 | Drama Supernatural | 43 | Music | 71 | Thriller |
| 16 | Ecchi | 44 | Mystery | 72 | Time Travel |
| 17 | Fantasy | 45 | Ninja | 73 | Traditional Games |
| 18 | Gender Bender | 46 | Office Workers | 74 | Traged |
| 19 | Genderswap | 47 | Philosophical | 75 | Tragedy |
| 20 | Ghosts | 48 | Police | 76 | Vampires |
| 21 | Girls' Love | 49 | Post-Apocalyptic | 77 | Video Games |
| 22 | Gore | 50 | Psychological | 78 | Villainess |
| 23 | Gyaru | 51 | Reincarnation | 79 | Virtual Reality |
| 24 | Harem | 52 | Reverse Harem | 80 | Wuxia |
| 25 | Historical | 53 | Romance | 81 | Yuri |
| 26 | Horror | 54 | Samurai | 82 | Zombies |
| 27 | Incest | 55 | School | | |
| 28 | Isekai | 56 | School Life | | |

---

## Status Map

| ID | Status |
|----|--------|
| 1 | Berjalan (Ongoing) |
| 2 | Tamat (Completed) |

---

## Write Operations

### Single Komik Write

```
write_komik(pool, detail)
  ├── upsert_komik(pool, detail)          → (komik_id, is_new)
  ├── upsert_chapters(pool, komik_id, chapters) → chapters_count
  └── sync_genres(pool, komik_id, genre_ids)  → genres_count
```

- `upsert_komik()`: INSERT ON CONFLICT DO UPDATE → atomic, no race condition
- `upsert_chapters()`: Multi-row INSERT dengan 500 rows/chunk dalam satu transaction
- `sync_genres()`: DELETE existing + multi-row INSERT (transactional)

### Batch Write (Phase 2)

```
batch_write_komik(pool, details[])
  ├── Phase 1: UNNEST batch upsert all komik (1 query)
  │   └── Build HashMap<slug, (komik_id, is_new)> for O(1) lookup
  ├── Phase 2: Multi-row INSERT chapters (500 rows/chunk)
  │   └── Single transaction for ALL chapter chunks
  └── Phase 3: Batch DELETE + multi-row INSERT genres (200 rows/chunk)
```

### Batch Update Latest Chapters

```
batch_update_latest_chapters(pool, [(komik_id, new_chapter_number)])
  └── UNNEST batch UPDATE (1 query for N updates)
```

---

## Read Operations

### Smart Update Support

```sql
-- Load chapter map for comparison
SELECT slug, id, latest_chapter_number::float8 AS latest_chapter_number
FROM komik ORDER BY id;
```

### Readback Verification

```bash
# Show recent komik
komikindo-scraper db-show --limit 10

# Show specific komik
komikindo-scraper db-show --slug "one-piece"

# Full detail as JSON
komikindo-scraper db-detail --slug "one-piece"

# Inspect schema
komikindo-scraper db-schema --table komik
```

---

## Schema Management

### Setup (Safe, Idempotent)

```bash
komikindo-scraper db-setup
```

Creates all tables, indexes, constraints, and triggers if they don't exist. Safe to run multiple times.

### Ensure (Minimal Column Add)

```bash
# Called automatically at startup
```

Adds `chapter_url` column to chapters table if missing. Safe ALTER TABLE with IF NOT EXISTS.

### Reset (Keep Schema, Clear Data)

```bash
komikindo-scraper db-reset
```

TRUNCATE semua tabel dengan CASCADE. Schema tetap ada, data dihapus. Identity counters di-reset.

### Drop All (DANGEROUS)

```bash
komikindo-scraper db-drop-all
```

DROP semua tabel secara permanen. Data dan schema hilang.

---

## Troubleshooting

### Connection Errors

| Error | Cause | Solution |
|-------|-------|----------|
| `Failed to connect` | Wrong URL format | Check `postgresql://user:pass@host:5432/db` |
| `SSL error` | Missing CA certs | Install `ca-certificates` |
| `too many connections` | Pool exhausted | Reduce `max_connections` or batch size |
| `prepared statement does not exist` | PgBouncer (port 6543) | Use port 5432 (auto-redirected) |

### Data Issues

| Issue | Cause | Solution |
|-------|-------|----------|
| Duplicate chapters | Missing UNIQUE constraint | Run `db-setup` to add constraint |
| `updated_at` not updating | Missing trigger | Run `db-setup` to create trigger |
| Chapter URLs missing | Old schema without `chapter_url` | Run `db-setup` to add column |
| Wrong chapter order | NUMERIC sorting | Use `ORDER BY chapter_number DESC` |

### Performance Issues

| Issue | Cause | Solution |
|-------|-------|----------|
| Slow batch inserts | Individual INSERTs | Use `batch_write_komik()` (UNNEST) |
| Slow chapter upserts | Per-row INSERT | Multi-row INSERT (500 rows/chunk) |
| Slow smart update | N individual UPDATEs | `batch_update_latest_chapters()` |
| Slow ALTER TABLE | Running during fetch | Move to startup (`ensure_schema`) |
