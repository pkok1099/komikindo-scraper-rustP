/// JSONL output helpers.
/// Format JSONL: 1 line per komik, crash-safe, resume-friendly.

use crate::parsers::KomikDetail;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Append satu komik ke JSONL file.
pub fn append_jsonl(filepath: &Path, komik: &KomikDetail) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(filepath)?;

    let json = serde_json::to_string(komik).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;

    writeln!(file, "{}", json)?;
    Ok(())
}

/// Append raw JSON string ke JSONL file.
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
            e.file_name()
                .to_string_lossy()
                .starts_with("full_fetch_")
                && e.file_name().to_string_lossy().ends_with(".jsonl")
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
