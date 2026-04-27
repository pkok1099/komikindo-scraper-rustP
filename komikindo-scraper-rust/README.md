# KomikIndo Scraper - Rust Version (Termux Compatible)

Scraper untuk [KomikIndo](https://komikindo.ch) yang ditulis ulang di Rust.
Full async, memory-efficient, dan bisa di-build langsung di Termux.

## ⚡ Perubahan dari Python Version

### FIX: Masalah Slug
**Sebelumnya (Python):**
```python
# chapter_read.py - URL di-construct manual, BISA SALAH!
chapter_url = build_chapter_url(slug, chapter_number)
# Contoh: build_chapter_url("nano-machine", 309)
#       -> "https://komikindo.ch/nano-machine-chapter-309/"
# Masalah: slug di daftar-manga bisa beda format dengan slug di chapter URL
```

**Sekarang (Rust):**
```rust
// parsers.rs - extract_chapters() -> URL LANGSUNG dari href di detail page
ChapterInfo {
    number: 309.0,
    url: "https://komikindo.ch/nano-machine-chapter-309/",  // ASLI dari website!
}

// scraper.rs - scrape_chapter_images() pakai URL langsung
scrape_chapter_images(&chapter.url, &fetcher).await
// Tidak ada lagi build_chapter_url() yang bisa salah!
```

### Keuntungan Rust vs Python
- **Single binary** - Tidak perlu install dependencies
- **Memory efficient** - ~10-20MB RAM vs ~100-200MB Python
- **Fast** - Native compiled, connection pooling via reqwest
- **Termux friendly** - rustls-tls (no OpenSSL), bisa build langsung di Android

## 📱 Install di Termux

```bash
# 1. Install Rust di Termux
pkg install rust

# 2. Clone repository
git clone <repo-url>
cd komikindo-scraper-rust

# 3. Build release
cargo build --release

# 4. Binary ada di:
ls target/release/komikindo-scraper
```

Atau cross-compile dari PC:
```bash
# Target: aarch64-linux-android (Android ARM64)
rustup target add aarch64-linux-android
# Lihat README-cross-compile.md untuk detail lengkap
```

## 🚀 Usage

### Full Fetch (Scrape Semua Komik)

```bash
# Full fetch semua komik (detail + chapter images)
./komikindo-scraper full-fetch

# Skip chapter images (lebih cepat, hanya metadata)
./komikindo-scraper full-fetch --skip-chapters

# Testing: limit 10 komik
./komikindo-scraper full-fetch --limit 10

# Turbo mode: 500 concurrent requests
./komikindo-scraper full-fetch --turbo

# Resume dari terakhir
./komikindo-scraper full-fetch --resume

# Dengan proxy
./komikindo-scraper full-fetch --proxy socks5://127.0.0.1:1080
```

### Homepage Check

```bash
./komikindo-scraper homepage
```

## 📂 Output

Hasil scrape disimpan di `data/` dalam format JSONL:
```
data/
└── full_fetch_20260427_090000.jsonl
```

Setiap baris = 1 komik (JSON), crash-safe, bisa resume.

### Contoh Output JSONL
```json
{
  "slug": "155895-nano-machine",
  "judul": "Nano Machine",
  "tipe": "Manhwa",
  "thumb_domain_id": 0,
  "thumb_path": "2022/09/50kg-Cinderella.jpg",
  "status_id": 1,
  "author": "한중월야",
  "rating": 8.5,
  "genre_list": ["Action", "Martial Arts"],
  "genre_ids": [1, 36],
  "chapters": [
    {
      "number": 309.0,
      "url": "https://komikindo.ch/nano-machine-chapter-309/",
      "cdn_domain_id": 1,
      "cdn_path_prefix": "data/91164060/16/abc",
      "image_filenames": ["SLePJKtMVn", "xyz789"],
      "image_ext_ids": [1, 1],
      "total_images": 20
    }
  ],
  "latest_chapter_number": 309.0
}
```

## 🔧 Environment Variables (.env)

```
PROXY_URL=socks5://127.0.0.1:1080
PROXY_ENABLED=true
SCRAPER_RETRIES=3
```

## 🏗️ Struktur Project

```
komikindo-scraper-rust/
├── Cargo.toml              # Dependencies & build config
├── src/
│   ├── main.rs             # CLI entry point, full fetch runner
│   ├── config.rs           # Constants, genre maps, URL builders
│   ├── fetcher.rs          # Async HTTP client (reqwest + rustls)
│   ├── parsers.rs          # HTML parsing (scraper crate)
│   ├── scraper.rs          # High-level scrape functions
│   └── jsonl.rs            # JSONL I/O helpers
└── data/                   # Output (JSONL files)
```

## 📊 Performance

| Metric | Python (aiohttp) | Rust (reqwest) |
|--------|-----------------|----------------|
| Binary Size | N/A (interpreter) | ~6.3 MB |
| RAM Usage | ~100-200 MB | ~10-20 MB |
| Startup Time | ~1-2s | ~0.01s |
| Dependencies | pip install 10+ packages | Single binary |
