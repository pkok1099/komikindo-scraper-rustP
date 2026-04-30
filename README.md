# KomikIndo Scraper (Rust)

Scraper performa tinggi untuk [KomikIndo](https://komikindo.ch), ditulis dalam Rust. Dirancang untuk scrape **8677+ komik** dari komikindo.id dan menyimpannya ke **Supabase PostgreSQL** dengan kecepatan target **300+ komik/menit**.

> **Method 2: Tanpa image** — hanya menyimpan metadata komik, daftar chapter (nomor + URL), dan genre. Tidak ada image scraping.

---

## Fitur Utama

- **Full Fetch Paralel** — Scrape semua 8677+ komik secara paralel dengan concurrency terkontrol (default 512 in-flight requests)
- **Smart Update** — Incremental update dari `/komik-terbaru/`, deteksi komik baru & chapter baru tanpa re-scrape seluruh detail
- **Two-Phase Pipeline** — Phase 1: fetch ke JSONL (ringan, crash-safe), Phase 2: batch import ke database
- **Supabase DB** — Upsert otomatis ke PostgreSQL (komik, chapters, genres, scrape log) dengan batch UNNEST
- **JSONL Backup** — Output crash-safe format JSONL sebagai fallback/backup, bisa di-resume
- **Cloudflare Bypass** — Menggunakan `curl` crate dengan TLS fingerprint Chrome, cookie jar, dan header realistis
- **Connection Reuse** — Thread-local curl handle cache dengan HTTP keep-alive, menghemat 100-200ms TCP+TLS handshake per request
- **Termux Ready** — Build langsung di Android tanpa OpenSSL dependency (static curl + rustls)
- **GitHub Actions** — Smart update cron setiap 6 jam + manual trigger, build release amd64 & arm64
- **Diagnostics** — Built-in `debug` dan `check` command untuk troubleshooting connectivity, DB, dan proxy

---

## Arsitektur

```
komikindo-scraper-rust/
├── Cargo.toml                  # Dependencies & build config (jemalloc, LTO, static curl)
├── .github/workflows/
│   ├── update.yml              # Cron setiap 6 jam → smart update → Supabase DB
│   └── release.yml             # Build release amd64 + arm64 (Termux)
├── src/
│   ├── main.rs                 # CLI entry point, full-fetch & update runner
│   ├── config.rs               # Constants, genre maps, URL builders, env config
│   ├── fetcher.rs              # Async HTTP client (curl + Cloudflare bypass + connection reuse)
│   ├── parsers.rs              # HTML parsing (scraper crate, LazyLock cached selectors)
│   ├── db.rs                   # Supabase PostgreSQL: upsert, batch UNNEST, schema ensure
│   ├── scraper.rs              # High-level scrape logic, smart update pagination
│   └── jsonl.rs                # JSONL read/write helpers (buffered, resume-friendly)
├── docs/
│   ├── ARCHITECTURE.md         # Arsitektur detail & data flow
│   ├── OPTIMIZATION.md         # Semua optimisasi yang diterapkan
│   ├── DATABASE.md             # Schema, migration, dan operasi database
│   ├── CLI.md                  # Referensi CLI lengkap
│   └── DEPLOYMENT.md           # Panduan deploy (Termux, Server, GitHub Actions)
├── data/                       # Output JSONL (gitignored)
└── .env                        # DATABASE_URL (gitignored)
```

> Lihat [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) untuk detail arsitektur dan data flow.

---

## Database Schema (Supabase PostgreSQL)

| Tabel | Deskripsi |
|-------|-----------|
| `komik` | Metadata: judul, slug, thumbnail, rating, sinopsis, author, chapter count |
| `chapters` | Daftar chapter per komik (chapter_number + chapter_url, tanpa image data) |
| `komik_genres` | Relasi many-to-many komik ↔ genre (82 genre hardcoded) |
| `scrape_log` | Log hasil scraping (operation, status, totals, timestamp) |

> Lihat [docs/DATABASE.md](docs/DATABASE.md) untuk detail schema, migration, dan contoh query.

---

## Quick Start

### Termux (Android ARM64)

```bash
pkg install rust git ca-certificates
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
# Binary: target/release/komikindo-scraper
```

### Download Binary (Pre-built)

Lihat [GitHub Releases](https://github.com/pkok1099/komikindo-scraper-rust/releases) untuk binary amd64 dan arm64 (Termux).

```bash
# Termux
pkg install ca-certificates
wget -q https://github.com/pkok1099/komikindo-scraper-rust/releases/latest/download/komikindo-scraper-termux
chmod +x komikindo-scraper-termux
```

### Linux / Server

```bash
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
```

---

## Usage

### Environment Variables

Buat file `.env` di root project:

```env
DATABASE_URL=postgresql://postgres.xxx@aws-1-ap-southeast-1.pooler.supabase.com:6543/postgres
PROXY_URL=socks5://127.0.0.1:1080
PROXY_ENABLED=1
SCRAPER_RETRIES=3
SCRAPER_TIMEOUT=30
```

> `DATABASE_URL` opsional. Tanpa DB, hasil scrape disimpan ke file JSONL.

### Commands

```bash
# --- Connectivity ---
komikindo-scraper check                              # Test koneksi ke komikindo.ch
komikindo-scraper check --proxy socks5://...         # Test dengan proxy
komikindo-scraper -v check                           # Verbose (curl protocol details)

# --- Diagnostics ---
komikindo-scraper debug                              # Run semua diagnostic
komikindo-scraper debug --db                         # Test DB connection saja
komikindo-scraper debug --network                    # Test network saja
komikindo-scraper debug --show-env                   # Lihat konfigurasi .env

# --- Full Fetch (Phase 1: ke JSONL) ---
komikindo-scraper full-fetch                         # Fetch semua komik ke JSONL
komikindo-scraper full-fetch --limit 50              # Batasi 50 komik saja
komikindo-scraper full-fetch --resume                # Resume dari JSONL terakhir
komikindo-scraper full-fetch --start-from "slug"     # Resume dari slug tertentu
komikindo-scraper full-fetch --auto-upload           # Auto upload ke DB setelah selesai

# --- Upload JSONL ke DB (Phase 2) ---
komikindo-scraper upload-db                          # Upload JSONL terbaru ke DB
komikindo-scraper upload-db --file data/xxx.jsonl    # Upload file tertentu
komikindo-scraper upload-db --batch-size 500         # Batch size (default: 1000)

# --- Smart Update (incremental) ---
komikindo-scraper update                             # Cek komik terbaru
komikindo-scraper update --db                        # Update + write ke DB
komikindo-scraper update --dry-run                   # Preview perubahan tanpa save
komikindo-scraper update --max-pages 5               # Fetch 5 halaman /komik-terbaru/
komikindo-scraper update --max-age-minutes 720       # Filter 12 jam terakhir

# --- DB Management ---
komikindo-scraper db-setup                           # Buat schema (tables, indexes, triggers)
komikindo-scraper db-show                            # Lihat data komik di DB
komikindo-scraper db-show --slug "one-piece"         # Detail komik tertentu
komikindo-scraper db-detail --slug "one-piece"       # Full detail (JSON)
komikindo-scraper db-schema --table komik            # Lihat schema tabel
komikindo-scraper db-reset                           # TRUNCATE semua data
komikindo-scraper db-drop-all                        # DROP semua tabel (DANGER!)

# --- Homepage ---
komikindo-scraper homepage                           # Cek update terbaru dari homepage

# --- Benchmark ---
komikindo-scraper bench-parse --kind list            # Benchmark parse komik list
komikindo-scraper bench-parse --kind detail --slug "one-piece"  # Benchmark parse detail
```

### Performance Tuning

```bash
# Tingkatkan concurrency (default: 512)
komikindo-scraper full-fetch --max-in-flight 1024 --max-blocking-threads 1024

# Kurangi concurrency untuk RAM/CPU terbatas (Termux)
komikindo-scraper full-fetch --max-in-flight 64 --max-blocking-threads 64

# Custom worker threads
komikindo-scraper full-fetch --worker-threads 4
```

> Lihat [docs/CLI.md](docs/CLI.md) untuk referensi lengkap semua command dan flag.

---

## Two-Phase Pipeline

Proyek ini menggunakan arsitektur **two-phase pipeline** untuk memisahkan fetching data dari database write:

### Phase 1: Fetch → JSONL

```
komikindo-scraper full-fetch
```

- Fetch semua 8677+ komik secara paralel ke file JSONL
- Tidak ada DB write — proses ringan dan cepat
- Crash-safe: setiap komik ditulis langsung ke JSONL (append-only)
- Resume: `--resume` atau `--start-from <slug>` untuk melanjutkan

### Phase 2: JSONL → Database

```
komikindo-scraper upload-db
```

- Baca JSONL dan batch insert ke Supabase PostgreSQL
- Menggunakan UNNEST untuk batch upsert (1 query untuk N komik)
- Batch size configurable (default: 1000)

### Alternative: Auto Upload

```bash
komikindo-scraper full-fetch --auto-upload
```

Otomatis upload ke DB setelah fetch selesai (single command).

---

## Optimisasi

Proyek ini telah dioptimasi secara ekstensif untuk performa scraping maksimal:

| Area | Optimisasi | Dampak |
|------|-----------|--------|
| **HTTP** | Thread-local curl handle cache (connection reuse) | Hemat 100-200ms TCP+TLS handshake per request |
| **HTTP** | TCP_NODELAY (disable Nagle's algorithm) | Hemat ~6-29 menit across 8677 requests |
| **HTTP** | HTTP/2 pipewait + keep-alive | Multiplexing pada koneksi yang sama |
| **HTTP** | FORBID_REUSE pada 429 + reset | Fresh connection tanpa rebuild handle |
| **HTTP** | DNS cache 3600s, maxage_conn 120s | Hindari repeated DNS + stale connection cleanup |
| **Parsing** | LazyLock cached selectors & regex | Zero per-call regex/selector compilation |
| **Parsing** | Pre-allocated Vec with capacity | Kurangi re-allocation |
| **I/O** | BufWriter 1MB + thread-local serialize buf | Amortized disk I/O |
| **I/O** | Tail-seek untuk last_slug_from_jsonl | Baca 64KB bukan 18MB |
| **Memory** | jemalloc global allocator | 5-15% improvement untuk allocation-heavy workloads |
| **Memory** | UnsafeCell zero-copy response | Vec<u8> → String tanpa clone |
| **Memory** | Arc<str> shared config | Hindari clone per request |
| **DB** | Batch UNNEST upsert | 1 query untuk N komik (bukan N query) |
| **DB** | Multi-row INSERT (500 rows/chunk) | Kurangi round-trips ke DB |
| **DB** | Direct port 5432 (bukan PgBouncer 6543) | Prepared statements support |
| **Concurrency** | Semaphore bounded concurrency | Kontrol in-flight requests |
| **Concurrency** | Chunked task spawning (200/chunk) | Hindari OOM dari spawning semua sekaligus |
| **Concurrency** | parking_lot::Mutex | ~30-50ns less overhead vs std::sync::Mutex |
| **Build** | LTO + codegen-units=1 + strip + panic=abort | Binary kecil (~6MB) dan optimal |

> Lihat [docs/OPTIMIZATION.md](docs/OPTIMIZATION.md) untuk penjelasan detail setiap optimisasi.

---

## Dependencies

| Crate | Fungsi |
|-------|--------|
| `curl` | HTTP client dengan TLS fingerprint Chrome, static-curl + rustls |
| `scraper` | HTML parsing (CSS selector) |
| `sqlx` | PostgreSQL async client (Supabase, rustls TLS) |
| `tokio` | Async runtime (worker + blocking thread pool configurable) |
| `clap` | CLI argument parser (derive macro) |
| `serde` / `serde_json` | Serialization / JSONL |
| `regex` | Regex dengan LazyLock caching |
| `chrono` | Date/time |
| `anyhow` / `thiserror` | Error handling |
| `parking_lot` | Lightweight Mutex (no poisoning, spin-then-park) |
| `tikv-jemallocator` | jemalloc global allocator (Linux/macOS only) |
| `dotenvy` | .env file loading |

---

## Performance Comparison

| Metric | Python (aiohttp) | Rust (curl, optimized) |
|--------|-----------------|------------------------|
| Binary Size | N/A (interpreter) | ~6 MB (stripped) |
| RAM Usage | ~100-200 MB | ~10-20 MB |
| Startup Time | ~1-2s | ~0.01s |
| Max Concurrent Requests | ~50-100 | 512+ (configurable) |
| Connection Reuse | Limited | Thread-local cache + keep-alive |
| Dependencies | pip install 10+ packages | Single binary (static) |
| Termux Compatible | Tidak stabil | Full support |
| Cloudflare Bypass | Unreliable | Chrome TLS fingerprint |

---

## Fix dari Python Version

### Masalah Slug (Fixed)

Python membangun chapter URL secara manual dari slug, yang sering salah format:

```python
# Python — BISA SALAH!
chapter_url = build_chapter_url("nano-machine", 309)
# → "https://komikindo.ch/nano-machine-chapter-309/"  # Tidak selalu benar
```

Rust mengambil URL langsung dari `<a href>` di detail page:

```rust
// Rust — URL ASLI dari website!
ChapterInfo {
    number: 309.0,
    url: "https://komikindo.ch/nano-machine-chapter-309/",  // dari href
}
```

### Bug Fixes (optimize-beta)

| Bug | Fix |
|-----|-----|
| CLI panic (`--env` conflict) | Rename arg ke `--env` dengan proper handling |
| Error 25P02 (aborted transaction) | Remove try-fallback in transaction |
| Error 42P10 (invalid column reference) | Add UNIQUE constraint migration |
| Slow ALTER TABLE on hot path | Move `ensure_schema` to startup |
| Process death/OOM | Chunked task spawning (200/chunk) |

---

## GitHub Actions

### Smart Update (Cron)

Workflow `update.yml` berjalan otomatis setiap **6 jam** (07:00, 13:00, 19:00, 01:00 WIB).

**Setup:**
1. Tambahkan secret `DATABASE_URL` di repo Settings > Secrets and variables > Actions
2. Workflow otomatis build, run update, dan upsert ke DB

**Manual trigger:** Actions tab > Smart Update > Run workflow

> **Note:** Update bisa gagal jika komikindo.ch memblokir datacenter IPs (Cloudflare challenge).

### Build Release

Workflow `release.yml` membuat binary untuk:
- **amd64** (`x86_64-unknown-linux-gnu`) — Linux PC/Server/Codespace
- **arm64** (`aarch64-linux-android`) — **Termux (Android)** — STATIC binary

Trigger: push tag `v*` atau manual dispatch.

---

## Documentation

| File | Deskripsi |
|------|-----------|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Arsitektur detail, data flow, module overview |
| [docs/OPTIMIZATION.md](docs/OPTIMIZATION.md) | Semua optimisasi yang diterapkan beserta justifikasi |
| [docs/DATABASE.md](docs/DATABASE.md) | Schema, migration, query examples, troubleshooting |
| [docs/CLI.md](docs/CLI.md) | Referensi CLI lengkap (semua command dan flag) |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Panduan deploy ke Termux, Server, dan GitHub Actions |

---

## License

MIT
