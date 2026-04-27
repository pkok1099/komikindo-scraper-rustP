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
        .map_err(|_| anyhow::anyhow!("Gagal fetch detail {}", komik_url))?;

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
        .map_err(|_| anyhow::anyhow!("Gagal fetch chapter: {}", chapter_url))?;

    Ok(parsers::parse_chapter_images(&html))
}

/// Scrape homepage untuk update terbaru (incremental).
pub async fn scrape_homepage_updates(fetcher: &Fetcher) -> Result<Vec<parsers::HomepageUpdate>> {
    println!("[HOME] Fetching homepage for incremental update check...");

    let html = fetcher.fetch_page(BASE_URL).await
        .map_err(|_| anyhow::anyhow!("Gagal fetch homepage"))?;

    let updates = parsers::parse_homepage_updates(&html);
    println!("[HOME] Found {} recently updated komik", updates.len());
    Ok(updates)
}
