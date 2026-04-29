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
///
/// FIXES applied:
///   1. upsert_komik: use INSERT ON CONFLICT DO UPDATE (atomic, no race condition)
///   2. upsert_chapters: use INSERT ON CONFLICT DO UPDATE (no DELETE+INSERT)
///   3. ensure_schema: called outside transaction
///   4. chapters: UNIQUE(komik_id, chapter_number) constraint
///   5. komik: updated_at trigger auto-update
///   6. Proper composite index on chapters(komik_id, chapter_number)

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};

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
// SCHEMA ENSURE (best-effort)
// ============================================================

/// Ensure minimal columns exist for web consumption.
/// Safe to run multiple times.
pub async fn ensure_schema(pool: &PgPool) -> Result<()> {
    // chapters.chapter_url is needed to reconstruct direct chapter links on the web.
    // If the user's schema doesn't have it yet, add it.
    let _ = sqlx::query("ALTER TABLE chapters ADD COLUMN IF NOT EXISTS chapter_url TEXT")
        .execute(pool)
        .await;

    Ok(())
}

/// Create required tables/constraints if they do not exist.
/// Intended for alpha/beta environments; safe to run repeatedly.
pub async fn setup_schema(pool: &PgPool) -> Result<()> {
    // komik
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS komik (
            id SERIAL PRIMARY KEY,
            slug TEXT UNIQUE NOT NULL,
            judul TEXT NOT NULL,
            thumb_domain_id SMALLINT NULL,
            thumb_path TEXT NULL,
            tipe TEXT NULL,
            status_id SMALLINT NULL,
            author TEXT NULL,
            artist TEXT NULL,
            alternative_title TEXT NULL,
            sinopsis TEXT NULL,
            rating DOUBLE PRECISION NULL,
            latest_chapter_number NUMERIC NULL,
            chapter_count INTEGER NULL,
            created_at TIMESTAMPTZ DEFAULT now(),
            updated_at TIMESTAMPTZ DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create table komik")?;

    // chapters (with UNIQUE constraint to prevent duplicate chapters)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS chapters (
            id BIGSERIAL PRIMARY KEY,
            komik_id INTEGER NOT NULL REFERENCES komik(id) ON DELETE CASCADE,
            chapter_number NUMERIC NOT NULL,
            chapter_url TEXT NULL,
            cdn_domain_id SMALLINT NULL,
            cdn_path_prefix TEXT NULL,
            image_filenames TEXT NULL,
            image_ext_id SMALLINT NULL,
            total_images SMALLINT DEFAULT 0,
            created_at TIMESTAMPTZ DEFAULT now(),
            updated_at TIMESTAMPTZ DEFAULT now(),
            UNIQUE (komik_id, chapter_number)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create table chapters")?;

    // komik_genres
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS komik_genres (
            komik_id INTEGER NOT NULL REFERENCES komik(id) ON DELETE CASCADE,
            genre_id SMALLINT NOT NULL,
            PRIMARY KEY (komik_id, genre_id)
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create table komik_genres")?;

    // scrape_log
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS scrape_log (
            id BIGSERIAL PRIMARY KEY,
            operation TEXT NOT NULL,
            status TEXT NOT NULL,
            total_processed INTEGER NOT NULL DEFAULT 0,
            total_new INTEGER NOT NULL DEFAULT 0,
            total_updated INTEGER NOT NULL DEFAULT 0,
            total_failed INTEGER NOT NULL DEFAULT 0,
            details TEXT NULL,
            created_at TIMESTAMPTZ DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create table scrape_log")?;

    // Helpful indexes (safe)
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_chapters_komik_id ON chapters(komik_id)")
        .execute(pool)
        .await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_chapters_num ON chapters(chapter_number)")
        .execute(pool)
        .await;
    // Composite index for the common lookup pattern (komik_id, chapter_number)
    // Note: UNIQUE constraint already creates an index, but this explicit one
    // can be optimized differently by the query planner for ORDER BY scenarios.
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_chapters_komik_num ON chapters(komik_id, chapter_number DESC)")
        .execute(pool)
        .await;

    // FIX: Ensure UNIQUE constraint on chapters(komik_id, chapter_number) exists.
    // The CREATE TABLE IF NOT EXISTS above won't add constraints to an existing table.
    // Use DO $$ block to safely add the constraint only if it doesn't exist.
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conname = 'chapters_komik_id_chapter_number_key'
            ) THEN
                -- First clean up any duplicate rows that would violate the constraint
                DELETE FROM chapters a
                USING chapters b
                WHERE a.id < b.id
                  AND a.komik_id = b.komik_id
                  AND a.chapter_number = b.chapter_number;

                ALTER TABLE chapters
                    ADD CONSTRAINT chapters_komik_id_chapter_number_key
                    UNIQUE (komik_id, chapter_number);
            END IF;
        END;
        $$"#,
    )
    .execute(pool)
    .await
    .context("Failed to ensure UNIQUE constraint on chapters(komik_id, chapter_number)")?;

    // Auto-update updated_at trigger for komik table
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION update_komik_updated_at()
        RETURNS TRIGGER AS $$
        BEGIN
            NEW.updated_at = now();
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create update_komik_updated_at function")?;

    // Drop and recreate trigger (idempotent)
    let _ = sqlx::query("DROP TRIGGER IF EXISTS trg_komik_updated_at ON komik")
        .execute(pool)
        .await;
    sqlx::query(
        r#"
        CREATE TRIGGER trg_komik_updated_at
            BEFORE UPDATE ON komik
            FOR EACH ROW
            EXECUTE FUNCTION update_komik_updated_at()
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create trg_komik_updated_at trigger")?;

    // Auto-update updated_at trigger for chapters table
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION update_chapters_updated_at()
        RETURNS TRIGGER AS $$
        BEGIN
            NEW.updated_at = now();
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create update_chapters_updated_at function")?;

    let _ = sqlx::query("DROP TRIGGER IF EXISTS trg_chapters_updated_at ON chapters")
        .execute(pool)
        .await;
    sqlx::query(
        r#"
        CREATE TRIGGER trg_chapters_updated_at
            BEFORE UPDATE ON chapters
            FOR EACH ROW
            EXECUTE FUNCTION update_chapters_updated_at()
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to create trg_chapters_updated_at trigger")?;

    Ok(())
}

// ============================================================
// UPSERT KOMIK
// ============================================================

/// UPSERT satu komik ke database (atomic, race-condition free).
/// Uses INSERT ... ON CONFLICT DO UPDATE for true upsert.
/// Returns (komik_id, is_new).
pub async fn upsert_komik(pool: &PgPool, detail: &KomikDetail) -> Result<(i32, bool)> {
    // Atomic UPSERT: single query, no race condition.
    // ON CONFLICT (slug) handles the unique constraint on slug.
    // EXCLUDED is the row we tried to insert (the "new" values).
    let row = sqlx::query(
        r#"
        INSERT INTO komik (
            slug, judul, thumb_domain_id, thumb_path, tipe,
            status_id, author, artist, alternative_title,
            sinopsis, rating, latest_chapter_number, chapter_count
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (slug) DO UPDATE SET
            judul = EXCLUDED.judul,
            thumb_domain_id = EXCLUDED.thumb_domain_id,
            thumb_path = EXCLUDED.thumb_path,
            tipe = EXCLUDED.tipe,
            status_id = EXCLUDED.status_id,
            author = EXCLUDED.author,
            artist = EXCLUDED.artist,
            alternative_title = EXCLUDED.alternative_title,
            sinopsis = EXCLUDED.sinopsis,
            rating = EXCLUDED.rating,
            latest_chapter_number = EXCLUDED.latest_chapter_number,
            chapter_count = EXCLUDED.chapter_count
        RETURNING id, (xmax = 0) AS is_new
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
    .context("Failed to upsert komik")?;

    let komik_id: i32 = row.get("id");
    // xmax = 0 means the row was inserted (not updated).
    // This is a PostgreSQL-specific trick: on INSERT, xmax is 0;
    // on UPDATE (via ON CONFLICT DO UPDATE), xmax is non-zero.
    let is_new: bool = row.try_get("is_new").unwrap_or(false);

    Ok((komik_id, is_new))
}

// ============================================================
// UPSERT CHAPTERS (batch per komik)
// ============================================================

/// UPSERT semua chapters untuk satu komik.
/// Uses INSERT ON CONFLICT DO UPDATE (atomic, preserves chapter IDs).
///
/// Method 2: hanya menyimpan chapter_number dan url.
/// image-related fields = NULL.
///
/// IMPORTANT: Caller MUST call db::setup_schema() + db::ensure_schema() once
/// before starting the pipeline. This function does NOT call ensure_schema
/// anymore to avoid slow DDL (ALTER TABLE) inside high-concurrency scenarios.
pub async fn upsert_chapters(pool: &PgPool, komik_id: i32, chapters: &[ChapterInfo]) -> Result<usize> {
    if chapters.is_empty() {
        return Ok(0);
    }

    // Use transaction for atomicity
    let mut tx = pool.begin().await.context("Failed to begin transaction")?;

    // UPSERT each chapter using INSERT ON CONFLICT DO UPDATE.
    // This preserves existing chapter IDs (unlike DELETE + INSERT).
    let mut upserted = 0usize;
    for ch in chapters {
        sqlx::query(
            r#"
            INSERT INTO chapters (komik_id, chapter_number, chapter_url)
            VALUES ($1, $2, $3)
            ON CONFLICT (komik_id, chapter_number) DO UPDATE SET
                chapter_url = EXCLUDED.chapter_url,
                updated_at = now()
            "#,
        )
        .bind(komik_id)
        .bind(ch.number)
        .bind(&ch.url)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Failed to upsert chapter {} for komik_id={}", ch.number, komik_id))?;

        upserted += 1;
    }

    tx.commit().await.context("Failed to commit transaction")?;

    Ok(upserted)
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
// BATCH WRITE (Phase 2: from JSONL → DB)
// ============================================================

/// Result dari batch write: jumlah new + updated.
#[derive(Debug, Default)]
pub struct BatchWriteResult {
    pub new_count: usize,
    pub updated_count: usize,
}

/// Batch write multiple komik details ke DB.
/// Optimized: batch upsert komik, then all chapters in one transaction, then genres.
pub async fn batch_write_komik(pool: &PgPool, details: &[KomikDetail]) -> Result<BatchWriteResult> {
    if details.is_empty() {
        return Ok(BatchWriteResult::default());
    }

    let mut result = BatchWriteResult::default();

    // Phase 1: Upsert all komik (one query each - unavoidable for ON CONFLICT RETURNING)
    let mut komik_ids: Vec<(String, i32, bool)> = Vec::with_capacity(details.len());
    for detail in details {
        let (komik_id, is_new) = upsert_komik(pool, detail).await?;
        if is_new {
            result.new_count += 1;
        } else {
            result.updated_count += 1;
        }
        komik_ids.push((detail.slug.clone(), komik_id, is_new));
    }

    // Phase 2: Batch all chapters into single transaction
    // Collect ALL (komik_id, chapter_number, chapter_url) triples
    let mut all_chapters: Vec<(i32, f64, String)> = Vec::new();
    for detail in details {
        if let Some((_, komik_id, _)) = komik_ids.iter().find(|(s, _, _)| s == &detail.slug) {
            for ch in &detail.chapters {
                all_chapters.push((*komik_id, ch.number, ch.url.clone()));
            }
        }
    }

    if !all_chapters.is_empty() {
        let mut tx = pool.begin().await.context("Failed to begin chapter batch transaction")?;

        for (komik_id, number, url) in &all_chapters {
            sqlx::query(
                r#"
                INSERT INTO chapters (komik_id, chapter_number, chapter_url)
                VALUES ($1, $2, $3)
                ON CONFLICT (komik_id, chapter_number) DO UPDATE SET
                    chapter_url = EXCLUDED.chapter_url,
                    updated_at = now()
                "#,
            )
            .bind(*komik_id)
            .bind(*number)
            .bind(url)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to batch upsert chapter {} for komik_id={}", number, komik_id))?;
        }

        tx.commit().await.context("Failed to commit chapter batch")?;
    }

    // Phase 3: Batch all genres
    // Collect all (komik_id, genre_id) pairs
    let mut all_genres: Vec<(i32, i16)> = Vec::new();
    for detail in details {
        if let Some((_, komik_id, _)) = komik_ids.iter().find(|(s, _, _)| s == &detail.slug) {
            for &gid in &detail.genre_ids {
                all_genres.push((*komik_id, gid));
            }
        }
    }

    if !all_genres.is_empty() {
        // Delete existing genres for these komik, then insert all
        let komik_id_list: Vec<i32> = komik_ids.iter().map(|(_, id, _)| *id).collect();
        let id_placeholders: Vec<String> = (1..=komik_id_list.len()).map(|i| format!("${i}")).collect();
        let delete_sql = format!(
            "DELETE FROM komik_genres WHERE komik_id IN ({})",
            id_placeholders.join(", ")
        );

        let mut delete_query = sqlx::query(&delete_sql);
        for id in &komik_id_list {
            delete_query = delete_query.bind(*id);
        }
        delete_query.execute(pool).await.context("Failed to batch delete genres")?;

        // Insert all genres
        let mut tx = pool.begin().await.context("Failed to begin genre batch transaction")?;
        for (komik_id, genre_id) in &all_genres {
            sqlx::query(
                "INSERT INTO komik_genres (komik_id, genre_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
            )
            .bind(*komik_id)
            .bind(*genre_id)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to insert genre {} for komik_id={}", genre_id, komik_id))?;
        }
        tx.commit().await.context("Failed to commit genre batch")?;
    }

    Ok(result)
}

// ============================================================
// READ HELPERS (for update comparison)
// ============================================================

/// Load slug → (komik_id, latest_chapter_number) dari DB.
/// Untuk smart update: compare dengan data dari /komik-terbaru/.
/// FIX: Only selects needed columns for efficiency.
pub async fn load_chapter_map(pool: &PgPool) -> Result<std::collections::HashMap<String, (i32, Option<f64>)>> {
    // Only fetch the 3 columns we need (slug, id, latest_chapter_number)
    // instead of SELECT * which fetches all columns including sinopsis etc.
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

// ============================================================
// READBACK (verify data persisted)
// ============================================================

#[derive(Debug, Clone)]
pub struct KomikRow {
    pub id: i32,
    pub slug: String,
    pub judul: String,
    pub latest_chapter_number: Option<f64>,
    pub chapter_count: Option<i32>,
}

pub async fn list_komik(pool: &PgPool, limit: i64) -> Result<Vec<KomikRow>> {
    let rows = sqlx::query(
        r#"
        SELECT
            id,
            slug,
            judul,
            latest_chapter_number::float8 AS latest_chapter_number,
            chapter_count
        FROM komik
        ORDER BY id DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to list komik")?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(KomikRow {
            id: row.get("id"),
            slug: row.get("slug"),
            judul: row.get("judul"),
            latest_chapter_number: row.get("latest_chapter_number"),
            chapter_count: row.try_get("chapter_count").ok(),
        });
    }
    Ok(out)
}

pub async fn get_komik_id_by_slug(pool: &PgPool, slug: &str) -> Result<Option<i32>> {
    let row = sqlx::query("SELECT id FROM komik WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await
        .context("Failed to get komik id by slug")?;
    Ok(row.map(|r| r.get::<i32, _>("id")))
}

pub async fn list_chapters(pool: &PgPool, komik_id: i32, limit: i64) -> Result<Vec<f64>> {
    let rows = sqlx::query(
        r#"
        SELECT chapter_number::float8 AS chapter_number
        FROM chapters
        WHERE komik_id = $1
        ORDER BY chapter_number DESC
        LIMIT $2
        "#,
    )
    .bind(komik_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to list chapters")?;

    Ok(rows
        .into_iter()
        .filter_map(|r| r.try_get::<f64, _>("chapter_number").ok())
        .collect())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChapterRow {
    pub number: f64,
    pub url: Option<String>,
}

pub async fn list_chapters_with_url(
    pool: &PgPool,
    komik_id: i32,
    limit: i64,
) -> Result<Vec<ChapterRow>> {
    // chapter_url may not exist on older schemas; try it first, fallback if needed.
    let q_with_url = sqlx::query(
        r#"
        SELECT
            chapter_number::float8 AS chapter_number,
            chapter_url
        FROM chapters
        WHERE komik_id = $1
        ORDER BY chapter_number DESC
        LIMIT $2
        "#,
    )
    .bind(komik_id)
    .bind(limit)
    .fetch_all(pool)
    .await;

    if let Ok(rows) = q_with_url {
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(ChapterRow {
                number: r.get("chapter_number"),
                url: r.try_get("chapter_url").ok(),
            });
        }
        return Ok(out);
    }

    let rows = sqlx::query(
        r#"
        SELECT chapter_number::float8 AS chapter_number
        FROM chapters
        WHERE komik_id = $1
        ORDER BY chapter_number DESC
        LIMIT $2
        "#,
    )
    .bind(komik_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to list chapters")?;

    Ok(rows
        .into_iter()
        .filter_map(|r| r.try_get::<f64, _>("chapter_number").ok())
        .map(|n| ChapterRow { number: n, url: None })
        .collect())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KomikDetailRow {
    pub id: i32,
    pub slug: String,
    pub judul: String,
    pub tipe: Option<String>,
    pub thumb_domain_id: Option<i16>,
    pub thumb_path: Option<String>,
    pub status_id: Option<i16>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub alternative_title: Option<String>,
    pub sinopsis: Option<String>,
    pub rating: Option<f64>,
    pub latest_chapter_number: Option<f64>,
    pub chapter_count: Option<i32>,
    pub genre_ids: Vec<i16>,
    pub chapters: Vec<ChapterRow>,
}

pub async fn get_komik_detail_by_slug(
    pool: &PgPool,
    slug: &str,
    chapters_limit: i64,
) -> Result<Option<KomikDetailRow>> {
    let row = sqlx::query(
        r#"
        SELECT
            id,
            slug,
            judul,
            tipe,
            thumb_domain_id,
            thumb_path,
            status_id,
            author,
            artist,
            alternative_title,
            sinopsis,
            rating,
            latest_chapter_number::float8 AS latest_chapter_number,
            chapter_count
        FROM komik
        WHERE slug = $1
        LIMIT 1
        "#,
    )
    .bind(slug)
    .fetch_optional(pool)
    .await
    .context("Failed to get komik by slug")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let komik_id: i32 = row.get("id");
    let genre_rows = sqlx::query("SELECT genre_id FROM komik_genres WHERE komik_id = $1 ORDER BY genre_id")
        .bind(komik_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let mut genre_ids = Vec::with_capacity(genre_rows.len());
    for r in genre_rows {
        if let Ok(g) = r.try_get::<i16, _>("genre_id") {
            genre_ids.push(g);
        }
    }

    let chapters = list_chapters_with_url(pool, komik_id, chapters_limit).await?;

    Ok(Some(KomikDetailRow {
        id: komik_id,
        slug: row.get("slug"),
        judul: row.get("judul"),
        tipe: row.try_get("tipe").ok(),
        thumb_domain_id: row.try_get("thumb_domain_id").ok(),
        thumb_path: row.try_get("thumb_path").ok(),
        status_id: row.try_get("status_id").ok(),
        author: row.try_get("author").ok(),
        artist: row.try_get("artist").ok(),
        alternative_title: row.try_get("alternative_title").ok(),
        sinopsis: row.try_get("sinopsis").ok(),
        rating: row.try_get("rating").ok(),
        latest_chapter_number: row.try_get("latest_chapter_number").ok(),
        chapter_count: row.try_get("chapter_count").ok(),
        genre_ids,
        chapters,
    }))
}

/// Get chapter count for a specific komik.
#[allow(dead_code)]
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
// UPDATE LATEST CHAPTER (for smart update without full re-scrape)
// ============================================================

/// Update latest_chapter_number for existing komik (smart update optimization).
/// Used when we detect a new chapter number from /komik-terbaru/ but don't
/// need to re-scrape the full detail page.
/// Returns true if the row was actually updated (i.e., the new number is higher).
pub async fn update_latest_chapter(
    pool: &PgPool,
    komik_id: i32,
    new_chapter_number: f64,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE komik
        SET latest_chapter_number = $1
        WHERE id = $2
          AND (latest_chapter_number IS NULL OR latest_chapter_number::float8 < $1)
        "#,
    )
    .bind(new_chapter_number)
    .bind(komik_id)
    .execute(pool)
    .await
    .context("Failed to update latest chapter number")?;

    Ok(result.rows_affected() > 0)
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
