# Optimisasi KomikIndo Scraper

Dokumen ini mencatat semua optimisasi yang telah diterapkan pada proyek komikindo-scraper-rust, beserta justifikasi dan estimasi dampak.

---

## Overview

Target: **300+ komik/menit** untuk scrape 8677+ komik dari komikindo.ch ke Supabase PostgreSQL.

Bottleneck utama:
1. **Network I/O** — HTTP request latency (~200-500ms per request)
2. **Cloudflare** — Rate limiting, challenge pages
3. **DB writes** — Round-trip ke Supabase (~100-300ms per query)
4. **Memory** — 8677+ concurrent tasks bisa menyebabkan OOM

---

## 1. HTTP Layer Optimizations

### 1.1 Thread-Local Curl Handle Cache (Connection Reuse)

**File:** `src/fetcher.rs`

**Problem:** Setiap `curl::Easy2::new()` melakukan TCP + TLS handshake baru (~100-200ms overhead). Dengan 8677 requests, ini berarti ~870-1734 detik (14-29 menit) hanya untuk handshakes.

**Solution:** Cache curl handle di thread-local storage. Setiap blocking thread mempertahankan handle-nya sendiri, sehingga koneksi HTTP keep-alive / HTTP/2 dipertahankan antar request.

```rust
thread_local! {
    static CACHED_CURL_HANDLE: RefCell<Option<Easy2<Collector>>> = RefCell::new(None);
}
```

**Impact:** Request pertama per thread membayar handshake cost, request berikutnya reuse koneksi (~0ms overhead). Dengan `max_blocking_threads == max_in_flight`, setiap thread menangani ~34 requests (8677/256), dan hanya request pertama yang mahal.

**Saved:** ~14-29 menit across 8677 requests (first request per thread only).

### 1.2 Handle Caching on HTTP Errors

**Problem:** Sebelumnya, handle di-drop pada semua error termasuk HTTP 429 dan content-type mismatch. Padahal TCP/TLS connection masih valid untuk request berikutnya.

**Solution:** Cache handle kembali SETELAH `perform()` berhasil, bahkan pada HTTP errors (429, content-type mismatch, CF challenge). Hanya drop handle pada network errors (connection may be dead).

```rust
// Cache handle AFTER successful perform()
{
    let collector = easy.get_ref();
    collector.reset();
}
*entry = Some(easy);  // Always cache back

// Validation happens AFTER caching — bail still preserves handle
```

**Impact:** Menghindari TCP+TLS handshake setelah HTTP error (429, dll). Pada scraping massal, 429 errors sangat umum.

### 1.3 FORBID_REUSE on 429 + Reset

**Problem:** Setelah HTTP 429 (rate limit), koneksi yang sama mungkin masih di-rate-limit. Tapi jika kita drop handle seluruhnya, kita harus rebuild dari nol.

**Solution:** Set `FORBID_REUSE(true)` pada response 429 — ini menutup koneksi yang di-rate-limit tapi mempertahankan handle. Request berikutnya membuka koneksi baru dengan handle yang sama (saves handle rebuild overhead). Set `FORBID_REUSE(false)` pada non-429 responses untuk memastikan keep-alive berfungsi normal.

```rust
if response_code == 429 {
    let _ = easy.forbid_reuse(true);   // Close this connection
} else {
    let _ = easy.forbid_reuse(false);  // Ensure keep-alive for next request
}
```

**Critical bug fix:** Sebelumnya, `forbid_reuse(true)` di-set tapi tidak pernah di-reset. Akibatnya, SETIAP request berikutnya juga menutup koneksinya, mengalahkan keep-alive secara permanen.

### 1.4 TCP_NODELAY (Disable Nagle's Algorithm)

**Problem:** Nagle's algorithm buffers small packets sampai ada ACK atau buffer penuh, menambah delay hingga 200ms per request.

**Solution:** Set `tcp_nodelay(true)` untuk mengirim HTTP request headers segera.

```rust
handle.tcp_nodelay(true).ok();
```

**Impact:** Dengan 8677 requests, penghematan bisa mencapai 6-29 menit jika setiap request mengalami Nagle delay.

### 1.5 HTTP/2 Pipewait

**Problem:** Tanpa pipewait, curl membuka koneksi baru untuk setiap request yang bisa di-multiplex di koneksi HTTP/2 yang sama.

**Solution:** Set `pipewait(true)` — curl menunggu sebentar untuk melihat apakah ada request lain yang bisa di-multiplex ke koneksi yang sama.

```rust
handle.pipewait(true).ok();
```

**Impact:** Lebih efisien untuk concurrent requests ke host yang sama (komikindo.ch).

### 1.6 DNS Cache + Connection Age

```rust
handle.dns_cache_timeout(Duration::from_secs(3600)).ok();  // 1 hour DNS cache
let _ = handle.maxage_conn(Duration::from_secs(120));       // Close idle conns after 120s
handle.tcp_keepalive(true).ok();                             // Enable TCP keepalive
handle.tcp_keepidle(Duration::from_secs(15)).ok();           // Start keepalive after 15s idle
```

- **DNS cache 3600s** — Menghindari DNS lookup berulang untuk host yang sama
- **maxage_conn 120s** — Menutup koneksi idle yang sudah lama, mencegah stale connection reuse
- **TCP keepalive** — Menjaga koneksi tetap hidup selama idle period

### 1.7 Accept-Encoding (Compression)

```rust
handle.accept_encoding("gzip, deflate, br").ok();
```

Mengaktifkan HTTP compression. Response HTML yang biasanya 50-200KB bisa di-compress menjadi 10-50KB, menghemat bandwidth dan waktu transfer.

### 1.8 Separate Connect Timeout

```rust
handle.connect_timeout(Duration::from_secs(timeout_secs.min(10))).ok();
```

Timeout terpisah untuk connection phase — fast-fail pada host yang unreachable tanpa menunggu full timeout (30s). Khususnya berguna saat target server down atau IP diblokir.

---

## 2. Parsing Optimizations

### 2.1 LazyLock Cached Selectors & Regex

**File:** `src/parsers.rs`

**Problem:** `Selector::parse()` dan `Regex::new()` melakukan kompilasi setiap dipanggil. Dengan 8677+ komik dan 15+ selectors per parse, ini berarti ~130,000+ kompilasi yang tidak perlu.

**Solution:** Semua selectors dan regex di-cache sebagai `LazyLock` statics — dikompilasi sekali saat pertama digunakan, lalu di-reuse selamanya.

```rust
static SEL_DETAIL_TITLES: LazyLock<Vec<Selector>> = LazyLock::new(|| {
    vec![
        Selector::parse("h1.titless").unwrap(),
        Selector::parse("h1.entry-title").unwrap(),
        Selector::parse("div.infox h1").unwrap(),
    ]
});

static RE_CHAPTER_NUM_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)chapter-([\d.]+)").unwrap());
```

**Impact:** Zero per-call regex/selector compilation. Parsing detail page turun dari ~0.5ms ke ~0.1ms per page.

### 2.2 Cached Genre Map

**Problem:** `build_genre_map()` membuat HashMap 82-entry setiap dipanggil.

**Solution:** Genre map di-cache sebagai `LazyLock` static:

```rust
static GENRE_MAP: LazyLock<HashMap<&'static str, i16>> = LazyLock::new(|| { ... });
```

**Impact:** Menghilangkan 82-entry HashMap allocation per komik. Dengan 8677 komik, ini menghemat ~8677 × 82 entry allocations.

### 2.3 Pre-allocated Vectors

Semua parser menggunakan `Vec::with_capacity()` untuk menghindari re-allocation:

```rust
let mut slugs = Vec::with_capacity(8000);      // komik list
let mut chapters = Vec::with_capacity(100);     // chapters per komik
let mut genre_ids = Vec::with_capacity(8);      // genres per komik
let mut items = Vec::with_capacity(40);         // terbaru items per page
```

### 2.4 No spawn_blocking for Detail Parsing

**Problem:** `tokio::task::spawn_blocking()` menambah overhead ~50μs per call. Dengan 8677 komik, total overhead = ~430ms.

**Solution:** Karena selectors sudah di-cache (LazyLock), parsing sangat cepat (~0.1ms) dan bisa berjalan langsung di async task tanpa spawn_blocking.

```rust
// Parse directly — no spawn_blocking needed
parsers::parse_komik_detail(&slug, &html)
    .ok_or_else(|| anyhow::anyhow!("Gagal parse detail untuk slug: {}", slug))
```

**Exception:** `parse_komik_list()` tetap menggunakan spawn_blocking karena list page sangat besar (~2MB HTML).

### 2.5 Cloudflare Check — First 2KB Only

```rust
let cf_check = &text[..text.len().min(2048)];
if cf_check.contains("Just a moment...") { ... }
```

Challenge pages selalu pendek. Hanya memindai 2KB pertama menghemat ~1.2GB string scanning across 8677 requests.

---

## 3. I/O Optimizations

### 3.1 Buffered JSONL Writer

**File:** `src/jsonl.rs`

**Problem:** `append_jsonl()` membuka dan menutup file setiap menulis satu baris — sangat tidak efisien.

**Solution:** `BufferedJsonlWriter` dengan `BufWriter` 1MB buffer dan `parking_lot::Mutex`:

```rust
pub struct BufferedJsonlWriter {
    writer: Mutex<BufWriter<File>>,
}
```

- File tetap terbuka selama proses berjalan
- BufWriter mengamortisasi disk I/O (flush otomatis saat buffer penuh)
- `parking_lot::Mutex` lebih ringan dari `std::sync::Mutex`

### 3.2 Thread-Local Serialization Buffer

```rust
pub fn append(&self, komik: &KomikDetail) -> std::io::Result<()> {
    thread_local! {
        static SERIALIZE_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(4096));
    }
    SERIALIZE_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();  // Clear but preserve capacity
        serde_json::to_writer(&mut *buf, komik)?;
        buf.push(b'\n');
        // Write to buffered file
        let mut writer = self.writer.lock();
        writer.write_all(&buf)?;
        Ok(())
    })
}
```

Reuses serialization buffer across calls — menghindari `Vec::new()` allocation per call (saves ~2μs/alloc).

### 3.3 Tail-Seek for last_slug_from_jsonl()

**Problem:** Mencari slug terakhir di file JSONL dengan 8000+ baris (18MB) membutuhkan baca seluruh file.

**Solution:** Seek ke 64KB terakhir file, baca hanya bagian tail, cari baris terakhir yang valid:

```rust
let read_size = 64 * 1024;
let seek_pos = if file_size > read_size as u64 {
    file_size - read_size as u64
} else {
    0
};
file.seek(SeekFrom::Start(seek_pos)).ok()?;
```

**Impact:** Mengurangi baca dari ~18MB ke ~64KB — 280x lebih cepat untuk resume operation.

---

## 4. Memory Optimizations

### 4.1 jemalloc Global Allocator

**File:** `Cargo.toml`, `src/main.rs`

```rust
#[cfg(all(unix, not(target_os = "android")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
```

jemalloc lebih efisien daripada system allocator untuk workloads dengan banyak small allocations (HTML parsing, string manipulation). Tidak diaktifkan di Android/Termux karena compatibility.

**Impact:** 5-15% improvement untuk allocation-heavy workloads.

### 4.2 UnsafeCell Zero-Copy Response

**File:** `src/fetcher.rs`

```rust
struct Collector {
    data: UnsafeCell<Vec<u8>>,
}

impl Collector {
    fn take_data(&self) -> Vec<u8> {
        unsafe { std::mem::take(&mut *self.data.get()) }
    }
}
```

Menggunakan `UnsafeCell<Vec<u8>>` untuk mengambil ownership data response tanpa clone. `std::mem::take()` mengosongkan Vec asli sambil mempertahankan capacity, lalu dikonversi ke String tanpa copy.

**Safety:** Hanya diakses antara `perform()` calls, single-threaded per handle.

### 4.3 Arc<str> for Shared Config

```rust
pub struct Fetcher {
    proxy_url: Arc<str>,           // Shared, no clone per request
    ca_bundle_path: Arc<Option<String>>,  // Shared, resolved once
    stats: Arc<AtomicFetcherStats>,       // Lock-free counters
    in_flight: Arc<Semaphore>,             // Shared semaphore
}
```

`Arc<str>` dan `Arc<Option<String>>` menghindari clone config per request. `Arc::clone()` hanya increment reference count (~1ns).

### 4.4 parking_lot::Mutex

```rust
// std::sync::Mutex: poisoning checks, ~80-100ns per lock/unlock
// parking_lot::Mutex: no poisoning, spin-then-park, ~30-50ns per lock/unlock
use parking_lot::Mutex;
```

Digunakan di `BufferedJsonlWriter` untuk mengurangi overhead per JSONL write.

---

## 5. Database Optimizations

### 5.1 Batch UNNEST Upsert

**File:** `src/db.rs`

**Problem:** Individual `INSERT ... ON CONFLICT` untuk setiap komik menghasilkan N round-trips ke DB. Dengan Supabase latency ~100-300ms per query dan 8677 komik, total = ~870-2600 detik (14-43 menit).

**Solution:** `batch_write_komik()` menggunakan `UNNEST` untuk insert N komik dalam satu query:

```sql
INSERT INTO komik (slug, judul, ...)
SELECT * FROM UNNEST(
    $1::text[], $2::text[], ...
)
ON CONFLICT (slug) DO UPDATE SET ...
RETURNING id, slug, (xmax = 0) AS is_new
```

**Impact:** Mengurangi DB round-trips dari N ke 1 per batch. Dengan batch size 1000, total queries = ~9 (bukan 8677).

### 5.2 Multi-Row INSERT (Chapters & Genres)

```sql
INSERT INTO chapters (komik_id, chapter_number, chapter_url) VALUES
    ($1,$2,$3), ($4,$5,$6), ($7,$8,$9), ...
ON CONFLICT (komik_id, chapter_number) DO UPDATE SET chapter_url = EXCLUDED.chapter_url
```

Chapters: 500 rows/chunk dalam satu transaction. Genres: 200 rows/chunk.

**Impact:** Mengurangi DB round-trips dari N×chapters ke ⌈total_chapters/500⌉.

### 5.3 Direct Port 5432 (Bypass PgBouncer)

PgBouncer (port 6543) tidak mendukung prepared statements yang digunakan sqlx. Kode otomatis redirect ke port 5432 (direct connection).

```rust
let direct_port = if port == 6543 { 5432 } else { port };
```

### 5.4 Schema Operations at Startup

`ensure_schema()` (ALTER TABLE) dan `setup_schema()` dipanggil sekali saat startup, bukan di dalam hot path. Ini menghindari slow DDL statements di tengah high-concurrency operations.

### 5.5 Batch Update Latest Chapters

```sql
UPDATE komik
SET latest_chapter_number = data.new_ch
FROM (SELECT * FROM UNNEST($1::integer[], $2::float8[])) AS data(id, new_ch)
WHERE komik.id = data.id
  AND (komik.latest_chapter_number IS NULL OR komik.latest_chapter_number::float8 < data.new_ch)
```

Single query untuk update N komik sekaligus — menggantikan N individual UPDATE queries. Dengan 50 updates, menghemat ~10s of pure DB latency.

### 5.6 xmax Trick for is_new Detection

```sql
RETURNING id, (xmax = 0) AS is_new
```

PostgreSQL-specific trick: pada INSERT baru, `xmax = 0`; pada UPDATE (via ON CONFLICT), `xmax ≠ 0`. Ini menghindari query terpisah untuk mengecek apakah row baru di-insert atau di-update.

---

## 6. Concurrency Optimizations

### 6.1 Semaphore Bounded Concurrency

```rust
let _permit = self.in_flight.clone().acquire_owned().await?;
```

Semaphore membatasi jumlah in-flight HTTP requests. Default: 512. Mencegah:
- Terlalu banyak open connections sekaligus
- Memory explosion dari 8677+ concurrent tasks
- Rate limiting dari target server

### 6.2 Chunked Task Spawning

**Problem:** Spawning 8677+ tokio tasks sekaligus menyebabkan OOM karena semua tasks langsung allocate memory.

**Solution:** Spawn tasks dalam chunks of 200. Tunggu setiap chunk selesai sebelum spawn chunk berikutnya.

```rust
for chunk in all_slugs.chunks(200) {
    let mut handles = Vec::with_capacity(chunk.len());
    for slug in chunk {
        handles.push(tokio::spawn(async move { ... }));
    }
    for handle in handles {
        let _ = handle.await;
    }
}
```

**Impact:** Memory usage tetap stabil, tidak ada OOM.

### 6.3 max_blocking_threads == max_in_flight

```bash
komikindo-scraper full-fetch --max-in-flight 512 --max-blocking-threads 512
```

Set jumlah blocking threads sama dengan in-flight limit. Ini memastikan setiap active request punya dedicated thread, sehingga thread-local curl handle cache bekerja optimal.

---

## 7. Build Optimizations

### 7.1 Cargo.toml Release Profile

```toml
[profile.release]
opt-level = 3          # Maximum optimization
lto = true             # Link-Time Optimization (cross-crate inlining)
codegen-units = 1      # Single codegen unit (better optimization, slower build)
strip = true           # Strip debug symbols
panic = "abort"        # Smaller binary, no unwind tables

[profile.release.package."*"]
opt-level = 2          # Dependencies: good optimization, faster compile
```

**Result:** Binary ~7-10MB self-contained (static-pie linked MUSL untuk amd64, statically linked untuk Termux), optimal runtime performance, zero glibc/OpenSSL dependency.

### 7.2 Static curl + rustls

```toml
curl = { version = "0.4", default-features = false, features = ["static-curl", "http2", "rustls"] }
```

- `static-curl` — Compile libcurl from source (no system libcurl dependency)
- `rustls` — Pure Rust TLS (no OpenSSL, works everywhere including Termux)
- `http2` — HTTP/2 support for multiplexing

### 7.3 Tokio Minimal Features

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
```

Hanya include features yang dibutuhkan — tidak ada `fs`, `net`, `process`, `signal` yang tidak digunakan.

---

## 8. Retry & Error Handling Optimizations

### 8.1 Smart Retry Backoff

```rust
let sleep_secs = if last_err.contains("RATE_LIMITED") {
    2.0                             // Longer backoff for rate limits
} else {
    0.3 * (attempt + 1) as f64     // Linear backoff for network errors
};
std::thread::sleep(Duration::from_secs_f64(sleep_secs));
```

Rate-limited requests mendapat backoff lebih lama (2s) karena server sedang membatasi. Network errors mendapat backoff pendek (0.3s × attempt) karena mungkin temporary.

### 8.2 Lazy Error Formatting

```rust
if verbose {
    eprintln!("[FETCH] Retry {}/{} for {}: {}",
        attempt + 1, max_retries, url, e);
} else {
    debug!("Retry {}/{} for {}: {}",
        attempt + 1, max_retries, url, e);
}
```

Format string hanya di-build ketika sebenarnya ditampilkan. `debug!()` macro juga skip formatting ketika log level < DEBUG.

---

## Summary: Estimated Total Savings

| Optimization | Estimated Savings |
|-------------|-------------------|
| Connection reuse (thread-local cache) | ~14-29 min |
| TCP_NODELAY | ~6-29 min |
| FORBID_REUSE fix | Prevents permanent keep-alive breakage |
| Cached selectors/regex | ~0.4ms × 8677 = ~3.5s parsing time |
| Batch UNNEST upsert | ~14-43 min DB round-trips |
| Tail-seek JSONL | 280x faster resume |
| jemalloc | 5-15% overall |
| Chunked spawning | Prevents OOM crashes |

**Note:** Actual performance depends on network conditions, server response time, and rate limiting behavior.
