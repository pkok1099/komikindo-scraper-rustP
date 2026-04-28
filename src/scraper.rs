/// Scraper functions untuk KomikIndo.

use anyhow::Result;
use crate::fetcher::Fetcher;
use crate::parsers;
use crate::config::{build_komik_url, BASE_URL};

/// Scrape full komik slug list dari daftar-manga page.
pub async fn scrape_full_komik_list(fetcher: &Fetcher) -> Result<Vec<String>> {
    let url = format!("{BASE_URL}/daftar-manga/?list");
    println!("[LIST] Fetching full komik list: {url}");

    let html = fetcher.fetch_page(&url).await?;
    let komik_list = parsers::parse_komik_list(&html);

    println!("[LIST] Found {} komik entries", komik_list.len());
    Ok(komik_list)
}

/// Scrape detail komik termasuk chapter list.
///
/// **PERBAIKAN SLUG**: Chapter URL diambil langsung dari halaman detail,
/// sehingga tidak perlu construct manual dari slug + chapter_number.
pub async fn scrape_komik_detail(slug: &str, fetcher: &Fetcher) -> Result<parsers::KomikDetail> {
    let komik_url = build_komik_url(slug);
    let html = fetcher.fetch_page(&komik_url).await
        .map_err(|e| anyhow::anyhow!("Gagal fetch detail {}: {e}", komik_url))?;

    parsers::parse_komik_detail(slug, &html)
        .ok_or_else(|| anyhow::anyhow!("Gagal parse detail untuk slug: {}", slug))
}

/// Scrape chapter image URLs dari chapter read page.
///
/// **PERBAIKAN**: `chapter_url` sudah dikirim langsung dari hasil parse detail page.
/// Tidak ada lagi build_chapter_url() yang bisa salah!
pub async fn scrape_chapter_images(
    chapter_url: &str,
    fetcher: &Fetcher,
) -> Result<parsers::ChapterImageData> {
    let html = fetcher.fetch_page(chapter_url).await
        .map_err(|e| anyhow::anyhow!("Gagal fetch chapter {}: {e}", chapter_url))?;

    Ok(parsers::parse_chapter_images(&html))
}

/// Scrape homepage untuk update terbaru (incremental).
pub async fn scrape_homepage_updates(fetcher: &Fetcher) -> Result<Vec<parsers::HomepageUpdate>> {
    println!("[HOME] Fetching homepage for incremental update check...");

    let html = fetcher.fetch_page(BASE_URL).await
        .map_err(|e| anyhow::anyhow!("Gagal fetch homepage: {e}"))?;

    let updates = parsers::parse_homepage_updates(&html);
    println!("[HOME] Found {} recently updated komik", updates.len());
    Ok(updates)
}

/// Scrape /komik-terbaru/ dengan pagination otomatis.
///
/// Logic: fetch page 1, cek item terakhir.
/// Kalau waktu terakhir < max_age_minutes → lanjut page 2, dst.
/// Berhenti saat item terakhir >= max_age_minutes atau max_pages tercapai.
pub async fn scrape_komik_terbaru(
    fetcher: &Fetcher,
    max_pages: u32,
    max_age_minutes: u32,
) -> Result<Vec<parsers::TerbaruItem>> {
    let mut all_items = Vec::new();
    let mut seen_slugs = std::collections::HashSet::new();

    for page in 1..=max_pages {
        let url = if page == 1 {
            format!("{BASE_URL}/komik-terbaru/")
        } else {
            format!("{BASE_URL}/komik-terbaru/page/{page}/")
        };

        println!("[TERBARU] Fetching page {page}: {url}");
        let html = match fetcher.fetch_page(&url).await {
            Ok(html) => html,
            Err(e) => {
                eprintln!("[TERBARU] Failed page {page}: {e}");
                if page == 1 {
                    return Err(e.context(format!(
                        "Website unreachable from this IP (page 1 failed). \
                         Common on datacenter IPs (GitHub Actions)."
                    )));
                }
                break;
            }
        };

        let items = parsers::parse_komik_terbaru(&html);
        println!("[TERBARU] Page {page}: {} items", items.len());

        if items.is_empty() {
            break;
        }

        // Cek item terakhir — kalau sudah melebihi max_age, stop pagination
        let last_time = items.last().map(|i| i.time_minutes).unwrap_or(0);
        if last_time >= max_age_minutes {
            // Hanya ambil item yang masih dalam rentang waktu
            for item in items {
                if item.time_minutes < max_age_minutes && seen_slugs.insert(item.slug.clone()) {
                    all_items.push(item);
                }
            }
            break;
        }

        // Semua item dalam rentang waktu — tambahkan semua
        for item in items {
            if seen_slugs.insert(item.slug.clone()) {
                all_items.push(item);
            }
        }
    }

    println!(
        "[TERBARU] Total: {} items (max_age={}min, pages_fetched={})",
        all_items.len(),
        max_age_minutes,
        all_items.len().div_ceil(40).min(max_pages as usize),
    );

    Ok(all_items)
}
