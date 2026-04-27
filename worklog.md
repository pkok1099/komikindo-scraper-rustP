---
Task ID: 1
Agent: Main Agent
Task: Konversi komikindo-scraper dari Python ke Rust + fix masalah slug

Work Log:
- Clone dan analisis project Python komikindo-scraper dari GitHub
- Baca semua source files: base.py, komik_list.py, komik_detail.py, chapter_read.py, parsers.py, homepage.py, full_fetch.py, incremental_update.py, config.py, schema.sql
- Identifikasi masalah slug: chapter URL di-construct manual via build_chapter_url(slug, chapter_number) di chapter_read.py, padahal detail page sudah punya href asli
- Buat project Rust baru: komikindo-scraper-rust/
- Konversi semua module: config.rs, fetcher.rs, parsers.rs, scraper.rs, jsonl.rs, main.rs
- Fix slug: extract_chapters() sekarang extract href langsung dari <a> tags di detail page dan simpan di ChapterInfo.url
- scrape_chapter_images() sekarang terima chapter_url langsung (bukan slug + chapter_number)
- Build berhasil: cargo build --release -> binary 6.3MB
- Termux compatible: rustls-tls (no OpenSSL), reqwest with socks support

Stage Summary:
- Project Rust siap di /home/z/my-project/komikindo-scraper-rust/
- Binary release di /home/z/my-project/download/komikindo-scraper
- Fix slug: chapter URL diambil langsung dari href di detail page (bukan construct manual)
- Commands: full-fetch, homepage (mirip Python version)
- Support: --skip-chapters, --limit, --resume, --turbo, --proxy
