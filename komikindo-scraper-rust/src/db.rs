/// Database module untuk Supabase PostgreSQL.
///
/// Method 2: TANPA image data.
/// Hanya menyimpan:
///   - komik metadata (judul, author, rating, sinopsis, dll)
///   - chapter list (number + url)
///   - genre relasi
///
/// Connection: PgBouncer pooler (6543) atau direct (5432).
/// Prepared statements disabled untuk PgBouncer compatibility.

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Executor, PgPool, Row};

use crate::parsers::{ChapterInfo, KomikDetail};

// ============================================================
// CONNECTION
// ============================================================

/// Connect ke Supabase PostgreSQL.
pub async fn connect(database_url: &str) -> Result<PgPool> {
    // Parse manual: extract host, port, user, pass, dbname
    let url = database_url.trim_start_matches("postgresql://");
    let (credentials, host_part) = url.split_once('@')
        .context("Invalid DATABASE_URL: missing '@' separator")?;
    let (user, password) = credentials.split_once(':')
        .context("Invalid DATABASE_URL: missing ':' in credentials")?;
    let (host_db, dbname) = host_part.rsplit_once('/')
        .context("Invalid DATABASE_URL: missing dbname")?;
    let (host_port, dbname) = if dbname.is_empty() {
        (host_db, "postgres")
    } else {
        (host_db, dbname)
    };
    let (host, port) = if host_port.contains(':') {
        let parts: Vec<&str> = host_port.rsplitn(2, ':').collect();
        (parts[1], parts[0].parse::<u16>().unwrap_or(5432))
    } else {
        (host_port, 5432)
    };

    // Use direct port 5432 for prepared statement support.
    // PgBouncer (6543) tidak support prepared statements dengan sqlx.
    let direct_port = if port == 6543 { 5432 } else { port };

    eprintln!("[DB] Connecting to {}:{} (direct port)...", host, direct_port);

    let options = PgConnectOptions::new()
        .host(host)
        .port(direct_port)
        .username(user)
        .password(password)
        .database(dbname)
        .ssl_mode(PgSslMode::Prefer);

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect_with(options)
        .await
        .map_err(|e| {
            eprintln!("[DB] Connection error: {:#}", e);
            e
        })
        .context("Failed to connect to Supabase PostgreSQL")?;

    // Test connection
    sqlx::query("SELECT 1").fetch_one(&pool).await
        .context("DB connection test failed")?;

    eprintln!("[DB] Connected!");
    Ok(pool)
}

// ============================================================
// UPSERT KOMIK
// ============================================================

/// UPSERT satu komik ke database.
/// Returns (komik_id, is_new).
pub async fn upsert_komik(pool: &PgPool, detail: &KomikDetail) -> Result<(i32, bool)> {
    // Check if exists first (Supabase direct connection works with simple queries)
    let existing: Option<(i32,)> = sqlx::query_as(
        "SELECT id FROM komik WHERE slug = $1"
    )
    .bind(&detail.slug)
    .fetch_optional(pool)
    .await
    .context("Failed to check existing komik")?;

    if let Some((komik_id,)) = existing {
        // UPDATE existing
        sqlx::query(
            r#"
            UPDATE komik SET
                judul = $1,
                thumb_domain_id = $2,
                thumb_path = $3,
                tipe = $4,
                status_id = $5,
                author = $6,
                artist = $7,
                alternative_title = $8,
                sinopsis = $9,
                rating = $10,
                latest_chapter_number = $11,
                chapter_count = $12,
                updated_at = now()
            WHERE id = $13
            "#,
        )
        .bind(&detail.judul.as_deref().unwrap_or(&detail.slug))
        .bind(detail.thumb_domain_id)
        .bind(&detail.thumb_path)
        .bind(&detail.tipe)
        .bind(detail.status_id)
        .bind(&detail.author)
        .bind(&detail.artist)
        .bind(&detail.alternative_title)
        .bind(&detail.sinopsis)
        .bind(detail.rating)
        .bind(detail.latest_chapter_number)
        .bind(detail.chapters.len() as i32)
        .bind(komik_id)
        .execute(pool)
        .await
        .context("Failed to update komik")?;

        Ok((komik_id, false))
    } else {
        // INSERT new — return generated id
        let row = sqlx::query(
            r#"
            INSERT INTO komik (
                slug, judul, thumb_domain_id, thumb_path, tipe,
                status_id, author, artist, alternative_title,
                sinopsis, rating, latest_chapter_number, chapter_count
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            RETURNING id
            "#,
        )
        .bind(&detail.slug)
        .bind(&detail.judul.as_deref().unwrap_or(&detail.slug))
        .bind(detail.thumb_domain_id)
        .bind(&detail.thumb_path)
        .bind(&detail.tipe)
        .bind(detail.status_id)
        .bind(&detail.author)
        .bind(&detail.artist)
        .bind(&detail.alternative_title)
        .bind(&detail.sinopsis)
        .bind(detail.rating)
        .bind(detail.latest_chapter_number)
        .bind(detail.chapters.len() as i32)
        .fetch_one(pool)
        .await
        .context("Failed to insert komik")?;

        let komik_id: i32 = row.get("id");
        Ok((komik_id, true))
    }
}

// ============================================================
// UPSERT CHAPTERS (batch per komik)
// ============================================================

/// UPSERT semua chapters untuk satu komik.
/// Uses batch insert untuk efisiensi.
///
/// Method 2: hanya menyimpan chapter_number dan url.
/// image-related fields = NULL.
pub async fn upsert_chapters(pool: &PgPool, komik_id: i32, chapters: &[ChapterInfo]) -> Result<usize> {
    if chapters.is_empty() {
        return Ok(0);
    }

    // Build batch insert
    // Gunakan transaction untuk atomicity
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    // Delete existing chapters for this komik (full replace)
    sqlx::query("DELETE FROM chapters WHERE komik_id = $1")
        .bind(komik_id)
        .execute(&mut *tx)
        .await
        .context("Failed to delete existing chapters")?;

    // Batch insert new chapters
    let mut inserted = 0usize;
    for ch in chapters {
        sqlx::query(
            r#"
            INSERT INTO chapters (
                komik_id, chapter_number
            ) VALUES ($1, $2)
            "#,
        )
        .bind(komik_id)
        .bind(ch.number)
        .execute(&mut *tx)
        .await
        .context("Failed to insert chapter")?;

        inserted += 1;
    }

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(inserted)
}

// ============================================================
// SYNC GENRES (batch per komik)
// ============================================================

/// Sync genres untuk satu komik.
/// Delete lama, insert baru.
pub async fn sync_genres(pool: &PgPool, komik_id: i32, genre_ids: &[i16]) -> Result<usize> {
    // Delete existing
    sqlx::query("DELETE FROM komik_genres WHERE komik_id = $1")
        .bind(komik_id)
        .execute(pool)
        .await
        .context("Failed to delete existing genres")?;

    // Insert new
    let mut inserted = 0usize;
    for &genre_id in genre_ids {
        sqlx::query(
            "INSERT INTO komik_genres (komik_id, genre_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(komik_id)
        .bind(genre_id)
        .execute(pool)
        .await
        .context("Failed to insert genre")?;

        inserted += 1;
    }

    Ok(inserted)
}

// ============================================================
// FULL WRITE (komik + chapters + genres)
// ============================================================

/// Write satu KomikDetail lengkap ke database.
/// Returns (komik_id, is_new, chapters_inserted, genres_synced).
pub async fn write_komik(pool: &PgPool, detail: &KomikDetail) -> Result<WriteResult> {
    // 1. UPSERT komik
    let (komik_id, is_new) = upsert_komik(pool, detail).await?;

    // 2. UPSERT chapters
    let chapters_count = upsert_chapters(pool, komik_id, &detail.chapters).await?;

    // 3. Sync genres
    let genres_count = sync_genres(pool, komik_id, &detail.genre_ids).await?;

    Ok(WriteResult {
        komik_id,
        is_new,
        chapters_inserted: chapters_count,
        genres_synced: genres_count,
    })
}

#[derive(Debug)]
pub struct WriteResult {
    pub komik_id: i32,
    pub is_new: bool,
    pub chapters_inserted: usize,
    pub genres_synced: usize,
}

// ============================================================
// READ HELPERS (for update comparison)
// ============================================================

/// Load slug → (komik_id, latest_chapter_number) dari DB.
/// Untuk smart update: compare dengan data dari /komik-terbaru/.
pub async fn load_chapter_map(pool: &PgPool) -> Result<std::collections::HashMap<String, (i32, Option<f64>)>> {
    // Cast NUMERIC → float8 to avoid type mismatch with Rust f64
    let rows = sqlx::query(
        "SELECT slug, id, latest_chapter_number::float8 AS latest_chapter_number FROM komik ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("Failed to load chapter map from DB")?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let slug: String = row.get("slug");
        let id: i32 = row.get("id");
        let latest: Option<f64> = row.get("latest_chapter_number");
        map.insert(slug, (id, latest));
    }

    Ok(map)
}

/// Get chapter count for a specific komik.
pub async fn get_chapter_count(pool: &PgPool, komik_id: i32) -> Result<i32> {
    let row = sqlx::query(
        "SELECT COUNT(*)::int AS cnt FROM chapters WHERE komik_id = $1",
    )
    .bind(komik_id)
    .fetch_one(pool)
    .await
    .context("Failed to get chapter count")?;

    Ok(row.get("cnt"))
}

// ============================================================
// SCRAPE LOG
// ============================================================

/// Log hasil scraping ke scrape_log table.
pub async fn log_scrape(
    pool: &PgPool,
    operation: &str,
    status: &str,
    total_processed: i32,
    total_new: i32,
    total_updated: i32,
    total_failed: i32,
    details: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO scrape_log (operation, status, total_processed, total_new, total_updated, total_failed, details)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(operation)
    .bind(status)
    .bind(total_processed)
    .bind(total_new)
    .bind(total_updated)
    .bind(total_failed)
    .bind(details)
    .execute(pool)
    .await
    .context("Failed to insert scrape log")?;

    Ok(())
}
