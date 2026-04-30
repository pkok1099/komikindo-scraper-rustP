# CLI Reference

Dokumen ini adalah referensi lengkap untuk semua command dan flag yang tersedia di komikindo-scraper.

---

## Global Flags

Flags yang berlaku untuk semua subcommands:

| Flag | Short | Default | Deskripsi |
|------|-------|---------|-----------|
| `--verbose` | `-v` | false | Enable verbose output (curl protocol details, timing, response info) |
| `--env <PATH>` | | | Specify `.env` file path (default: search CWD, binary dir, parent dirs) |
| `--worker-threads <N>` | | auto | Tokio worker threads (CPU cores). Default = Tokio auto-detect |
| `--max-blocking-threads <N>` | | 512 | Max blocking threads untuk libcurl `spawn_blocking` |
| `--max-in-flight <N>` | | 512 | Max in-flight HTTP requests (semaphore bound) |

### Performance Tuning Tips

```bash
# High performance (server with good bandwidth)
komikindo-scraper --max-in-flight 1024 --max-blocking-threads 1024 full-fetch

# Low resource (Termux / limited RAM)
komikindo-scraper --max-in-flight 64 --max-blocking-threads 64 full-fetch

# Custom CPU threads
komikindo-scraper --worker-threads 4 full-fetch
```

---

## Environment Variables

| Variable | Default | Deskripsi |
|----------|---------|-----------|
| `DATABASE_URL` | (none) | PostgreSQL connection string (Supabase). Kosong = JSONL mode only |
| `PROXY_URL` | (none) | Default proxy URL (e.g., `socks5://127.0.0.1:1080`) |
| `PROXY_ENABLED` | (none) | Set to `1`, `true`, or `yes` to enable default proxy |
| `SCRAPER_RETRIES` | 3 | Max retry attempts per request |
| `SCRAPER_TIMEOUT` | 30 | Request timeout in seconds |

---

## Subcommands

### `full-fetch`

Fetch semua komik + detail (FULL SPEED). Method 2: tanpa image scraping. Data disimpan ke JSONL file, TANPA auto upload ke DB.

```bash
komikindo-scraper full-fetch [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--limit <N>` | 0 | Batasi jumlah komik (0 = semua) |
| `--start-from <slug>` | | Resume dari slug tertentu |
| `--resume` | false | Resume dari JSONL terakhir (auto-detect last slug) |
| `--timeout <SEC>` | 30 | Timeout per request dalam detik |
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL (e.g., `socks5://127.0.0.1:1080`) |
| `--auto-upload` | false | Automatically upload to DB after fetch completes |

**Examples:**

```bash
# Fetch semua komik
komikindo-scraper full-fetch

# Fetch hanya 50 komik
komikindo-scraper full-fetch --limit 50

# Resume dari JSONL terakhir
komikindo-scraper full-fetch --resume

# Resume dari slug tertentu
komikindo-scraper full-fetch --start-from "nano-machine"

# Fetch + auto upload ke DB
komikindo-scraper full-fetch --auto-upload

# Fetch dengan proxy
komikindo-scraper full-fetch --proxy socks5://127.0.0.1:1080

# Fetch dengan custom concurrency
komikindo-scraper --max-in-flight 256 --max-blocking-threads 256 full-fetch
```

**Output:** JSONL file di `data/full_fetch_YYYYMMDD_HHMMSS.jsonl`

---

### `update`

Smart incremental update dari `/komik-terbaru/`. Bandingkan chapter number dengan DB, detect komik baru & chapter baru.

```bash
komikindo-scraper update [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--max-pages <N>` | 3 | Max halaman `/komik-terbaru/` yang di-fetch |
| `--max-age-minutes <N>` | 360 | Batas usia entry dalam menit (default 6 jam) |
| `--timeout <SEC>` | 30 | Timeout per request dalam detik |
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL |
| `--dry-run` | false | Hanya tampilkan perubahan, tanpa save |
| `--db` | false | Write ke database (requires DATABASE_URL) |

**Examples:**

```bash
# Preview perubahan tanpa save
komikindo-scraper update --dry-run

# Update + write ke DB
komikindo-scraper update --db

# Update 5 halaman terbaru, filter 12 jam
komikindo-scraper update --max-pages 5 --max-age-minutes 720 --db

# Update dengan proxy
komikindo-scraper update --db --proxy socks5://127.0.0.1:1080
```

**Smart Update Logic:**
1. Fetch `/komik-terbaru/` pages
2. Load chapter map from DB (slug → latest chapter)
3. Komik baru → scrape full detail → insert ke DB
4. Chapter baru → update `latest_chapter_number` saja
5. No changes → skip

---

### `upload-db`

Upload JSONL data ke database (Phase 2: batch insert). Gunakan setelah `full-fetch` selesai.

```bash
komikindo-scraper upload-db [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--file <PATH>` | auto | Path ke file JSONL (default: terbaru di `data/`) |
| `--batch-size <N>` | 1000 | Batch size untuk DB insert |

**Examples:**

```bash
# Upload JSONL terbaru ke DB
komikindo-scraper upload-db

# Upload file tertentu
komikindo-scraper upload-db --file data/full_fetch_20240101_120000.jsonl

# Upload dengan batch size lebih kecil
komikindo-scraper upload-db --batch-size 500
```

---

### `homepage`

Scrape homepage untuk cek update terbaru.

```bash
komikindo-scraper homepage [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL |

**Example:**

```bash
komikindo-scraper homepage
```

---

### `check`

Test connectivity ke komikindo.ch. Gunakan `--verbose` untuk melihat curl protocol details.

```bash
komikindo-scraper check [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL |
| `--timeout <SEC>` | 30 | Timeout per request dalam detik |

**Examples:**

```bash
# Basic connectivity test
komikindo-scraper check

# Test dengan proxy
komikindo-scraper check --proxy socks5://127.0.0.1:1080

# Verbose output (curl protocol details)
komikindo-scraper -v check
```

---

### `debug`

Run diagnostics: env config, .env loading, DB connection, network test, binary info.

```bash
komikindo-scraper debug [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--all` | false | Run semua tests |
| `--db` | false | Test DB connection only |
| `--network` | false | Test network only (direct + env proxy) |
| `--show-env` | false | Show environment/config info only |
| `--info` | false | Show binary/platform info only |
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL (overrides PROXY_URL) |
| `--timeout <SEC>` | 30 | Timeout per request dalam detik |

**Examples:**

```bash
# Run semua diagnostics
komikindo-scraper debug

# Test DB connection saja
komikindo-scraper debug --db

# Test network saja
komikindo-scraper debug --network

# Lihat konfigurasi .env
komikindo-scraper debug --show-env

# Binary/platform info
komikindo-scraper debug --info
```

---

### `bench-parse`

Benchmark parsing speed (fetch once, parse N times).

```bash
komikindo-scraper bench-parse --kind <KIND> [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--kind <KIND>` | required | What to benchmark: `list`, `homepage`, `terbaru`, `detail` |
| `--iters <N>` | 200 | Iterations (parse repeats) |
| `--slug <SLUG>` | | Slug untuk `--kind detail` (default: "one-piece") |
| `--timeout <SEC>` | 30 | Timeout per request dalam detik |
| `--proxy <URL>` | | SOCKS5/HTTP proxy URL |

**Examples:**

```bash
# Benchmark parse komik list
komikindo-scraper bench-parse --kind list

# Benchmark parse detail (default slug: one-piece)
komikindo-scraper bench-parse --kind detail

# Benchmark parse detail dengan slug custom
komikindo-scraper bench-parse --kind detail --slug "nano-machine"

# Benchmark dengan 500 iterations
komikindo-scraper bench-parse --kind terbaru --iters 500
```

---

### `db-show`

Query DB dan tampilkan data yang tersimpan (readback verification).

```bash
komikindo-scraper db-show [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--limit <N>` | 5 | Limit rows to show |
| `--slug <SLUG>` | | Show specific komik by slug |
| `--chapters <N>` | 5 | Show last N chapters per komik |

**Examples:**

```bash
# Show 5 komik terbaru
komikindo-scraper db-show

# Show 20 komik
komikindo-scraper db-show --limit 20

# Show specific komik
komikindo-scraper db-show --slug "one-piece"

# Show 10 chapters per komik
komikindo-scraper db-show --limit 10 --chapters 10
```

---

### `db-detail`

Print full komik detail dari DB (JSON) by slug.

```bash
komikindo-scraper db-detail --slug <SLUG> [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--slug <SLUG>` | required | Slug komik |
| `--chapters <N>` | 200 | Limit number of chapters returned |

**Example:**

```bash
komikindo-scraper db-detail --slug "one-piece"
```

---

### `db-schema`

Inspect DB schema (tables/columns) tanpa psql.

```bash
komikindo-scraper db-schema [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--table <TABLE>` | chapters | Which table: `komik`, `chapters`, `komik_genres`, `scrape_log` |

**Examples:**

```bash
komikindo-scraper db-schema --table komik
komikindo-scraper db-schema --table chapters
```

---

### `db-setup`

Setup DB schema — create tables, columns, indexes, triggers if missing. Safe to run multiple times.

```bash
komikindo-scraper db-setup
```

---

### `db-reset`

Reset DB data — TRUNCATE main tables. Schema tetap ada, data dihapus.

```bash
komikindo-scraper db-reset [OPTIONS]
```

| Option | Default | Deskripsi |
|--------|---------|-----------|
| `--restart-identity` | true | Also reset identity/serial counters |

---

### `db-drop-all`

Drop all scraper tables (DANGEROUS) — removes tables completely. Data dan schema hilang permanen.

```bash
komikindo-scraper db-drop-all
```

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error (network, parse, DB) |
| 2 | CLI parse error (invalid args) |

---

## Common Workflows

### First-Time Setup

```bash
# 1. Build
cargo build --release

# 2. Setup database
echo "DATABASE_URL=postgresql://..." > .env
./target/release/komikindo-scraper db-setup

# 3. Test connectivity
./target/release/komikindo-scraper debug

# 4. Run full fetch
./target/release/komikindo-scraper full-fetch

# 5. Upload ke DB
./target/release/komikindo-scraper upload-db

# 6. Verify
./target/release/komikindo-scraper db-show --limit 10
```

### Daily Update

```bash
# Smart update (hanya komik yang berubah)
./target/release/komikindo-scraper update --db
```

### Resume Interrupted Fetch

```bash
# Auto-resume dari JSONL terakhir
./target/release/komikindo-scraper full-fetch --resume

# Manual resume dari slug tertentu
./target/release/komikindo-scraper full-fetch --start-from "nano-machine"
```

### Full Fetch + Auto Upload (One Command)

```bash
./target/release/komikindo-scraper full-fetch --auto-upload
```
