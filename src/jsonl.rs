/// JSONL output helpers.
/// Format JSONL: 1 line per komik, crash-safe, resume-friendly.

use crate::parsers::KomikDetail;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ============================================================
// BUFFERED JSONL WRITER (persistent, avoids open/close per line)
// ============================================================

/// Thread-safe buffered JSONL writer.
/// Keeps the file open and uses a BufWriter for amortized I/O.
/// Much faster than append_jsonl() which opens/closes per line.
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
    pub fn append(&self, komik: &KomikDetail) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(komik).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    /// Append a raw JSON string (for error entries, etc.)
    pub fn append_raw(&self, json_str: &str) -> std::io::Result<()> {
        let mut writer = self.writer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        writeln!(writer, "{}", json_str)?;
        Ok(())
    }

    /// Flush any remaining buffered data to disk.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut writer = self.writer.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        writer.flush()?;
        Ok(())
    }

    /// Get the file path (for display purposes).
    #[allow(dead_code)]
    pub fn path(&self) -> String {
        String::new()
    }
}

/// Append satu komik ke JSONL file.
#[allow(dead_code)]
pub fn append_jsonl(filepath: &Path, komik: &KomikDetail) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(filepath)?;

    let mut bytes = serde_json::to_vec(komik).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    Ok(())
}

/// Append raw JSON string ke JSONL file.
#[allow(dead_code)]
pub fn append_jsonl_raw(filepath: &Path, json_str: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(filepath)?;
    writeln!(file, "{}", json_str)?;
    Ok(())
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
pub fn last_slug_from_jsonl(filepath: &Path) -> Option<String> {
    if !filepath.exists() {
        return None;
    }

    let file = File::open(filepath).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut last_line = String::new();

    for line in reader.lines() {
        if let Ok(line) = line {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                last_line = trimmed.to_string();
            }
        }
    }

    if last_line.is_empty() {
        return None;
    }

    serde_json::from_str::<serde_json::Value>(&last_line)
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
