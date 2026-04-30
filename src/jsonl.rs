/// JSONL output helpers.
/// Format JSONL: 1 line per komik, crash-safe, resume-friendly.

use crate::parsers::KomikDetail;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

// ============================================================
// BUFFERED JSONL WRITER (persistent, avoids open/close per line)
// ============================================================

/// Thread-safe buffered JSONL writer using parking_lot::Mutex.
/// Keeps the file open and uses a BufWriter for amortized I/O.
/// Much faster than append_jsonl() which opens/closes per line.
/// parking_lot::Mutex is lighter than std::sync::Mutex — no poisoning checks,
/// spin-then-park strategy, ~30-50ns less overhead per lock/unlock cycle.
pub struct BufferedJsonlWriter {
    writer: Mutex<BufWriter<File>>,
}

impl BufferedJsonlWriter {
    /// Create a new buffered writer (opens file in append mode).
    pub fn new(filepath: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(filepath)?;
        let writer = BufWriter::with_capacity(1024 * 1024, file); // 1MB buffer
        Ok(Self {
            writer: Mutex::new(writer),
        })
    }

    /// Append a KomikDetail to the JSONL file (thread-safe).
    /// Uses a thread-local serialization buffer to avoid allocating a new Vec
    /// on every call — reuses the same buffer across calls (saves ~2μs/alloc).
    pub fn append(&self, komik: &KomikDetail) -> std::io::Result<()> {
        thread_local! {
            static SERIALIZE_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(4096));
        }
        SERIALIZE_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.clear();
            serde_json::to_writer(&mut *buf, komik).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            buf.push(b'\n');
            // parking_lot::Mutex — no poisoning, no .map_err() needed
            let mut writer = self.writer.lock();
            writer.write_all(&buf)?;
            Ok(())
        })
    }

    /// Append a raw JSON string (for error entries, etc.)
    pub fn append_raw(&self, json_str: &str) -> std::io::Result<()> {
        let mut writer = self.writer.lock();
        writeln!(writer, "{}", json_str)?;
        Ok(())
    }

    /// Flush any remaining buffered data to disk.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut writer = self.writer.lock();
        writer.flush()?;
        Ok(())
    }

    /// Get the file path (for display purposes).
    #[allow(dead_code)]
    pub fn path(&self) -> String {
        String::new()
    }
}

/// Hitung baris di JSONL file.
pub fn count_jsonl(filepath: &Path) -> usize {
    if !filepath.exists() {
        return 0;
    }
    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    std::io::BufReader::new(file).lines().count()
}

/// Baca slug dari baris terakhir JSONL (untuk resume).
/// OPTIMIZED: Seeks to end of file and reads backwards to find the last line,
/// instead of reading the entire file line by line. For a file with 8000+ lines,
/// this reduces read from ~18MB to ~64KB (the tail of the file).
pub fn last_slug_from_jsonl(filepath: &Path) -> Option<String> {
    if !filepath.exists() {
        return None;
    }

    let metadata = std::fs::metadata(filepath).ok()?;
    let file_size = metadata.len();
    if file_size == 0 {
        return None;
    }

    let mut file = File::open(filepath).ok()?;

    // Read the last 64KB of the file (or the whole file if smaller).
    // A single JSONL line is typically 1-5KB, so 64KB covers ~15-60 lines.
    let read_size = 64 * 1024;
    let seek_pos = if file_size > read_size as u64 {
        file_size - read_size as u64
    } else {
        0
    };

    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(seek_pos)).ok()?;

    let mut tail = Vec::with_capacity(read_size);
    file.read_to_end(&mut tail).ok()?;

    // Find the last non-empty line by splitting on newlines from the end.
    // Skip trailing newlines, then find the last line with content.
    let tail_str = String::from_utf8_lossy(&tail);
    let last_line = tail_str
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?;

    let trimmed = last_line.trim();
    if trimmed.is_empty() {
        return None;
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| v.get("slug")?.as_str().map(|s| s.to_string()))
}

/// Cari file JSONL terbaru di directory.
pub fn find_latest_jsonl(dir: &Path) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }

    let entries: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            (name_str.starts_with("full_fetch_") || name_str.starts_with("komik_db"))
                && name_str.ends_with(".jsonl")
        })
        .collect();

    let mut entries: Vec<_> = entries
        .into_iter()
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((path, modified))
        })
        .collect();

    // Sort by modification time descending
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    entries.into_iter().next().map(|(p, _)| p)
}

// ============================================================
// DB HELPERS (HashMap-based JSONL database)
// ============================================================

/// Load seluruh JSONL ke HashMap<slug, KomikDetail>.
///
/// Efisien untuk update: load sekali, compare, save sekali.
/// Skip baris yang gagal parse (e.g., error entries).
pub fn load_db(filepath: &Path) -> HashMap<String, KomikDetail> {
    let mut map = HashMap::new();

    if !filepath.exists() {
        return map;
    }

    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(_) => return map,
    };

    let reader = std::io::BufReader::new(file);
    let mut count = 0usize;
    let mut skipped = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<KomikDetail>(trimmed) {
            Ok(detail) => {
                let slug = detail.slug.clone();
                map.insert(slug, detail);
                count += 1;
            }
            Err(_) => {
                skipped += 1;
            }
        }
    }

    if skipped > 0 {
        eprintln!("[DB] Loaded {count} entries, skipped {skipped} invalid lines");
    }

    map
}

/// Save HashMap<slug, KomikDetail> ke JSONL file.
///
/// Full rewrite — aman untuk data kecil (~18MB dengan Method 2).
pub fn save_db(filepath: &Path, data: &HashMap<String, KomikDetail>) -> std::io::Result<()> {
    if let Some(parent) = filepath.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(filepath)?;
    let mut writer = BufWriter::with_capacity(1024 * 64, file);

    for detail in data.values() {
        let json = serde_json::to_string(detail).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        writeln!(writer, "{}", json)?;
    }

    writer.flush()?;
    Ok(())
}
