# KomikIndo Scraper (Rust)

Scraper untuk [KomikIndo](https://komikindo.ch) yang ditulis di Rust. Dirancang untuk berjalan di **Termux (Android)** dan **GitHub Actions**.

> **Method 2: Tanpa image** — hanya menyimpan metadata komik, daftar chapter (nomor + URL), dan genre. Tidak ada image scraping.

## Fitur

- **Full Speed** — tidak ada concurrency limit, 8192 blocking threads, bottleneck hanya di internet
- **Smart Update** — incremental update dari halaman `/komik-terbaru/`, deteksi komik baru & chapter baru
- **Supabase DB** — upsert otomatis ke PostgreSQL (komik, chapters, genres, scrape log)
- **JSONL Backup** — output crash-safe format JSONL sebagai fallback/backup
- **Cloudflare Bypass** — menggunakan `curl` crate dengan TLS fingerprint Chrome
- **Termux Ready** — build langsung di Android tanpa OpenSSL dependency
- **GitHub Actions** — cron setiap 6 jam + manual trigger, build & deploy otomatis

## Database Schema (Supabase PostgreSQL)

| Tabel | Deskripsi |
|-------|-----------|
| `komik` | Metadata: judul, slug, thumbnail, rating, sinopsis, author, chapter count |
| `chapters` | Daftar chapter per komik (chapter_number only, tanpa image) |
| `komik_genres` | Relasi many-to-many komik ↔ genre |
| `scrape_log` | Log hasil scraping (operation, status, totals) |

## Install

### Termux (Android ARM64)

```bash
pkg install rust git
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
# Binary: target/release/komikindo-scraper
```

### Download Binary (Pre-built)

Lihat [GitHub Releases](https://github.com/pkok1099/komikindo-scraper-rust/releases) untuk binary amd64 dan arm64.

### Linux / Server

```bash
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
```

## Usage

### Environment Variables

Buat file `.env` di root project:

```env
DATABASE_URL=postgresql://postgres.xxx@aws-1-ap-southeast-1.pooler.supabase.com:6543/postgres
```

> `DATABASE_URL` opsional. Tanpa DB, hasil scrape disimpan ke file JSONL.

### Commands

```bash
# Smart update (cek komik terbaru, upsert ke DB)
./komikindo-scraper update

# Smart update dengan database
./komikindo-scraper update --db

# Smart update - dry run (lihat apa yang akan berubah)
./komikindo-scraper update --dry-run

# Smart update - 5 halaman, filter 12 jam terakhir
./komikindo-scraper update --max-pages 5 --max-age-minutes 720

# Full fetch semua komik
./komikindo-scraper full-fetch

# Full fetch - limit 50 komik
./komikindo-scraper full-fetch --limit 50

# Full fetch ke database
./komikindo-scraper full-fetch --db

# Full fetch - resume dari slug tertentu
./komikindo-scraper full-fetch --start-from "nano-machine"

# Homepage check
./komikindo-scraper homepage
```

### Full CLI Reference

```
komikindo-scraper [COMMAND]

Commands:
  full-fetch    Fetch semua komik + detail (FULL SPEED)
  update        Smart incremental update dari /komik-terbaru/
  homepage      Scrape homepage untuk cek update terbaru

Options:
  --db              Write ke database (requires DATABASE_URL)
  --limit <N>       Batasi jumlah komik (0 = semua)
  --start-from <slug>  Resume dari slug tertentu
  --resume          Resume dari JSONL terakhir
  --max-pages <N>   Max halaman /komik-terbaru/ (default: 3)
  --max-age-minutes <N>  Filter entry terbaru dalam N menit (default: 360)
  --dry-run         Hanya tampilkan perubahan, tanpa save
  --proxy <URL>     SOCKS5/HTTP proxy
  --timeout <SEC>   Timeout per request (default: 30)
```

## GitHub Actions

### Smart Update (Cron)

Workflow `update.yml` berjalan otomatis setiap **6 jam** (07:00, 13:00, 19:00, 01:00 WIB).

**Setup:**
1. Tambahkan secret `DATABASE_URL` di repo Settings > Secrets and variables > Actions
2. Workflow akan otomatis build, run update, dan push hasil ke branch `data`

**Manual trigger:** Actions tab > Smart Update > Run workflow

### Build Release

Workflow `release.yml` membuat binary untuk amd64 dan arm64. Trigger manual atau push tag `v*`.

## Arsitektur

```
komikindo-scraper-rust/
├── Cargo.toml              # Dependencies & build config
├── .github/workflows/
│   ├── update.yml          # Cron setiap 6 jam → Supabase DB
│   └── release.yml         # Build release amd64 + arm64
├── src/
│   ├── main.rs             # CLI entry point, full fetch & update runner
│   ├── config.rs           # Constants, genre maps, URL builders, env config
│   ├── db.rs               # Supabase PostgreSQL: upsert, sync, log
│   ├── fetcher.rs          # Async HTTP client (curl crate + Cloudflare bypass)
│   ├── parsers.rs          # HTML parsing (scraper crate)
│   ├── scraper.rs          # High-level scrape logic, smart update
│   └── jsonl.rs            # JSONL read/write helpers
├── data/                   # Output JSONL (gitignored)
└── .env                    # DATABASE_URL (gitignored)
```

## Dependencies

| Crate | Fungsi |
|-------|--------|
| `curl` | HTTP client, TLS fingerprint Chrome, Cloudflare bypass |
| `scraper` | HTML parsing (CSS selector) |
| `sqlx` | PostgreSQL async client (Supabase) |
| `tokio` | Async runtime, 8192 blocking threads |
| `clap` | CLI argument parser |
| `serde` / `serde_json` | Serialization / JSONL |
| `chrono` | Date/time |
| `anyhow` | Error handling |

## Performance

| Metric | Python (aiohttp) | Rust (curl) |
|--------|-----------------|-------------|
| Binary Size | N/A (interpreter) | ~6 MB (stripped) |
| RAM Usage | ~100-200 MB | ~10-20 MB |
| Startup Time | ~1-2s | ~0.01s |
| Concurrent Requests | ~50-100 | 8192 |
| Dependencies | pip install 10+ packages | Single binary |
| Termux Compatible | Tidak stabil | Full support |

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

## License

MIT
