use serde::Serialize;
use sqlx::{SqlitePool, prelude::FromRow};
use std::env;

const DEFAULT_SQLITE_DB_NAME: &str = "data/cache.db";
const SQLITE_DB_ENV: &str = "CACHE_DB_PATH";

pub async fn init() -> Result<SqlitePool, sqlx::Error> {
    let db_path = env::var(SQLITE_DB_ENV).unwrap_or_else(|_| DEFAULT_SQLITE_DB_NAME.to_string());

    // `mode=rwc` lets SQLite create the db file itself, but not missing parent
    // dirs — create those so a fresh checkout/CWD doesn't fail to open.
    if let Some(parent) = std::path::Path::new(&db_path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).expect("failed to create cache db directory");
    }

    let url = format!("sqlite://{}?mode=rwc", db_path);

    let conn = SqlitePool::connect(&url)
        .await
        .expect("FAILED TO CONNECT TO DB");

    create_search_cache(&conn)
        .await
        .expect("FAILED TO INITIALIZE DB");

    Ok(conn)
}

async fn create_search_cache(conn: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
    -- Engines
    CREATE TABLE IF NOT EXISTS engines (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE
    );

    -- Queries
    CREATE TABLE IF NOT EXISTS queries (
        id INTEGER PRIMARY KEY,
        query TEXT NOT NULL,
        engine_id INTEGER NOT NULL REFERENCES engines(id),
        fetched_at DATETIME DEFAULT CURRENT_TIMESTAMP NOT NULL  
    );
    
    -- Results
    CREATE TABLE IF NOT EXISTS results (
        id INTEGER PRIMARY KEY,
        url TEXT NOT NULL UNIQUE,
        title TEXT NOT NULL ,
        description TEXT NOT NULL 
    );
    
    -- Junction table: maps query -> result
    CREATE TABLE IF NOT EXISTS query_results (
        query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
        result_id INTEGER NOT NULL REFERENCES results(id),
        result_index INTEGER NOT NULL, -- preserves ordering in the page
        PRIMARY KEY (query_id, result_id)
    );

    -- Image Results
    CREATE TABLE IF NOT EXISTS images (
        id INTEGER PRIMARY KEY,
        url TEXT NOT NULL UNIQUE,
        title TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS query_images (
        query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
        image_id INTEGER NOT NULL REFERENCES images(id),
        image_index INTEGER NOT NULL,
        PRIMARY KEY (query_id, image_id)
    );
        "#,
    )
    .execute(conn)
    .await?;

    Ok(())
}

/// Atomically get-or-create the query row and insert `entries` for it.
/// Everything below runs on `tx` — never `pool` directly — so a failure
/// partway through rolls back instead of leaving a partial cache entry.
pub async fn upsert_query_with_results(
    pool: &SqlitePool,
    engine: &str,
    query: &str,
    entries: Vec<ResultRow>,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let engine_id = get_or_create_engine(&mut tx, engine).await?;
    let query_id = get_or_create_query(&mut tx, query, engine_id, fetched_at).await?;

    let current_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM query_results WHERE query_id = ?")
            .bind(query_id)
            .fetch_one(&mut *tx)
            .await?;

    for (i, entry) in entries.iter().enumerate() {
        let res =
            sqlx::query("INSERT OR IGNORE INTO results (url, title, description) VALUES (?, ?, ?)")
                .bind(&entry.url)
                .bind(&entry.title)
                .bind(&entry.description)
                .execute(&mut *tx)
                .await?;
        let result_id = if res.rows_affected() == 0 {
            sqlx::query_scalar("SELECT id FROM results WHERE url = ?")
                .bind(&entry.url)
                .fetch_one(&mut *tx)
                .await?
        } else {
            res.last_insert_rowid()
        };

        sqlx::query(
            "INSERT OR IGNORE INTO query_results (query_id, result_id, result_index) VALUES (?, ?, ?)",
        )
        .bind(query_id)
        .bind(result_id)
        .bind(current_count + i as i64)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(query_id)
}

/// Same contract as [`upsert_query_with_results`], for images.
pub async fn upsert_query_with_images(
    pool: &SqlitePool,
    engine: &str,
    query: &str,
    entries: Vec<ImagesRow>,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let engine_id = get_or_create_engine(&mut tx, engine).await?;
    let query_id = get_or_create_query(&mut tx, query, engine_id, fetched_at).await?;

    let current_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM query_images WHERE query_id = ?")
            .bind(query_id)
            .fetch_one(&mut *tx)
            .await?;

    for (i, entry) in entries.iter().enumerate() {
        let res = sqlx::query("INSERT OR IGNORE INTO images (url, title) VALUES (?, ?)")
            .bind(&entry.url)
            .bind(&entry.title)
            .execute(&mut *tx)
            .await?;
        let image_id = if res.rows_affected() == 0 {
            sqlx::query_scalar("SELECT id FROM images WHERE url = ?")
                .bind(&entry.url)
                .fetch_one(&mut *tx)
                .await?
        } else {
            res.last_insert_rowid()
        };

        sqlx::query(
            "INSERT OR IGNORE INTO query_images (query_id, image_id, image_index) VALUES (?, ?, ?)",
        )
        .bind(query_id)
        .bind(image_id)
        .bind(current_count + i as i64)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(query_id)
}

// Transaction-bound duplicates of get_engine_id/get_query below: sharing one
// generic fn over both `&SqlitePool` and `&mut Transaction` runs into an
// async/HRTB limitation once called through the boxed engine futures in lib.rs.
async fn get_or_create_engine(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    engine: &str,
) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM engines WHERE name = ?")
        .bind(engine)
        .fetch_optional(&mut **tx)
        .await?;

    if let Some((id,)) = row {
        return Ok(id);
    }

    Ok(sqlx::query("INSERT INTO engines (name) VALUES (?)")
        .bind(engine)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid())
}

async fn get_or_create_query(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    query: &str,
    engine_id: i64,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM queries WHERE query = ? AND engine_id = ?")
            .bind(query)
            .bind(engine_id)
            .fetch_optional(&mut **tx)
            .await?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    Ok(
        sqlx::query("INSERT INTO queries (query, engine_id, fetched_at) VALUES (?, ?, ?)")
            .bind(query)
            .bind(engine_id)
            .bind(fetched_at)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid(),
    )
}

pub async fn get_engine_id(pool: &SqlitePool, engine: &str) -> Result<i64, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM engines WHERE name = ?")
        .bind(engine)
        .fetch_optional(pool)
        .await?;

    if let Some((id,)) = row {
        return Ok(id);
    }

    let id = sqlx::query("INSERT INTO engines (name) VALUES (?)")
        .bind(engine)
        .execute(pool)
        .await?
        .last_insert_rowid();

    Ok(id)
}

#[derive(FromRow)]
pub struct QueryRow {
    pub id: i64,
}

pub async fn get_query(
    pool: &SqlitePool,
    query: &str,
    engine_id: i64,
) -> Result<Option<QueryRow>, sqlx::Error> {
    let row: Option<QueryRow> =
        sqlx::query_as("SELECT id FROM queries WHERE query = ? AND engine_id = ?")
            .bind(query)
            .bind(engine_id)
            .fetch_optional(pool)
            .await?;

    Ok(row)
}

pub async fn insert_query(
    pool: &SqlitePool,
    query: &str,
    engine_id: i64,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let id = sqlx::query("INSERT INTO queries (query, engine_id, fetched_at) VALUES (?, ?, ?)")
        .bind(query)
        .bind(engine_id)
        .bind(fetched_at)
        .execute(pool)
        .await?
        .last_insert_rowid();

    Ok(id)
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ImagesRow {
    pub url: String,
    pub title: String,
}

pub async fn get_images_for_query(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<ImagesRow>, sqlx::Error> {
    let rows: Vec<ImagesRow> = sqlx::query_as(
        r#"
        SELECT i.url, i.title
        FROM images i
        INNER JOIN query_images ir ON i.id = ir.image_id
        WHERE ir.query_id = ?
        ORDER BY ir.image_index ASC
        "#,
    )
    .bind(query_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn insert_image(pool: &SqlitePool, title: &str, url: &str) -> Result<i64, sqlx::Error> {
    let res = sqlx::query("INSERT OR IGNORE INTO images (url, title) VALUES (?, ?)")
        .bind(url)
        .bind(title)
        .execute(pool)
        .await?;

    if res.rows_affected() == 0 {
        let row: (i64,) = sqlx::query_as("SELECT id FROM images WHERE url = ?")
            .bind(url)
            .fetch_one(pool)
            .await?;
        Ok(row.0)
    } else {
        Ok(res.last_insert_rowid())
    }
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ResultRow {
    pub url: String,
    pub title: String,
    pub description: String,
}

pub async fn get_results_for_query(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<ResultRow>, sqlx::Error> {
    let rows: Vec<ResultRow> = sqlx::query_as(
        r#"
        SELECT r.url, r.title, r.description
        FROM results r
        INNER JOIN query_results qr ON r.id = qr.result_id
        WHERE qr.query_id = ?
        ORDER BY qr.result_index ASC
        "#,
    )
    .bind(query_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

#[derive(sqlx::FromRow)]
pub struct QueryImageRow {
    pub query_id: i64,
    pub image_id: i64,
}

pub async fn insert_query_image(
    pool: &SqlitePool,
    query_id: i64,
    image_id: i64,
    image_index: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO query_images (query_id, image_id, image_index) VALUES (?, ?, ?)",
    )
    .bind(query_id)
    .bind(image_id)
    .bind(image_index)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_image_for_query(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<QueryImageRow>, sqlx::Error> {
    sqlx::query_as("SELECT query_id, image_id FROM query_images WHERE query_id = ?")
        .bind(query_id)
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod test {
    use crate::cache::{
        ImagesRow, ResultRow, create_search_cache, get_engine_id, get_image_for_query,
        get_images_for_query, get_results_for_query, insert_image, insert_query,
        insert_query_image, upsert_query_with_images, upsert_query_with_results,
    };
    use chrono::Utc;
    use sqlx::SqlitePool;

    async fn new_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        create_search_cache(&pool).await.unwrap();

        pool
    }

    fn sample_results() -> Vec<ResultRow> {
        vec![
            ResultRow {
                url: "https://example.com".into(),
                title: "Example 1".into(),
                description: "First description".into(),
            },
            ResultRow {
                url: "https://super.com".into(),
                title: "Example 2".into(),
                description: "Second description".into(),
            },
            ResultRow {
                url: "https://mega.com".into(),
                title: "Example 3".into(),
                description: "Third description".into(),
            },
        ]
    }

    #[sqlx::test]
    async fn smoke_init_db() {
        let _ = new_db().await;
    }

    #[sqlx::test]
    async fn test_upsert_query_with_results() {
        let pool = new_db().await;
        let results = sample_results();

        let query = "rust sqlite test";
        let fetched_at = Utc::now().naive_utc();

        // upsert the query and results
        let query_id =
            upsert_query_with_results(&pool, "Brave", query, results.clone(), fetched_at)
                .await
                .expect("Failed to upsert query");

        assert!(query_id > 0);

        // retrieve results for this query
        let fetched = get_results_for_query(&pool, query_id).await.unwrap();
        assert_eq!(fetched.len(), results.len());

        for (i, r) in results.iter().enumerate() {
            assert_eq!(fetched[i].url, r.url);
            assert_eq!(fetched[i].title, r.title);
            assert_eq!(fetched[i].description, r.description);
        }
    }

    #[sqlx::test]
    async fn test_dedup_results() {
        let pool = new_db().await;
        let results = sample_results();

        let query = "dedup test";
        let fetched_at = Utc::now().naive_utc();

        // first insert
        let first_id =
            upsert_query_with_results(&pool, "Brave", query, results.clone(), fetched_at)
                .await
                .unwrap();

        // second insert with same query/results
        let second_id =
            upsert_query_with_results(&pool, "Brave", query, results.clone(), fetched_at)
                .await
                .unwrap();

        // should return same query_id
        assert_eq!(first_id, second_id);

        let fetched = get_results_for_query(&pool, first_id).await.unwrap();
        assert_eq!(fetched.len(), results.len());
    }

    #[sqlx::test]
    async fn test_append_results() {
        let pool = new_db().await;
        let page1 = sample_results();
        let page2 = vec![
            ResultRow {
                url: "https://extra.com".into(),
                title: "Extra 1".into(),
                description: "Extra description".into(),
            },
            ResultRow {
                url: "https://more.com".into(),
                title: "Extra 2".into(),
                description: "More description".into(),
            },
        ];

        let query = "pagination test";
        let fetched_at = Utc::now().naive_utc();

        // Insert page 1
        let query_id =
            upsert_query_with_results(&pool, "DuckDuckGo", query, page1.clone(), fetched_at)
                .await
                .unwrap();

        // Append page 2
        upsert_query_with_results(&pool, "DuckDuckGo", query, page2.clone(), fetched_at)
            .await
            .unwrap();

        // Verify all results
        let fetched = get_results_for_query(&pool, query_id).await.unwrap();
        assert_eq!(fetched.len(), page1.len() + page2.len());
        assert_eq!(fetched[0].url, page1[0].url);
        assert_eq!(fetched.last().unwrap().url, page2.last().unwrap().url);
    }

    #[sqlx::test]
    async fn test_insert_image_and_dedup() {
        let pool = new_db().await;

        let title = "Test Image";
        let url = "https://example.com/img.png";

        // First insert
        let id1 = insert_image(&pool, title, url)
            .await
            .expect("first insert failed");

        assert!(id1 > 0);

        // Second insert (should dedup)
        let id2 = insert_image(&pool, title, url)
            .await
            .expect("second insert failed");

        assert_eq!(id1, id2);
    }

    #[sqlx::test]
    async fn test_insert_query_image() {
        let pool = new_db().await;

        // Insert engine & query
        let engine_id = get_engine_id(&pool, "Brave").await.unwrap();
        let fetched_at = chrono::Utc::now().naive_utc();
        let query_id = insert_query(&pool, "image-query", engine_id, fetched_at)
            .await
            .unwrap();

        // Insert image
        let image_id = insert_image(&pool, "img-title", "https://img.com")
            .await
            .unwrap();

        // Insert mapping
        insert_query_image(&pool, query_id, image_id, 0)
            .await
            .unwrap();

        // Fetch back
        let rows = get_image_for_query(&pool, query_id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].query_id, query_id);
        assert_eq!(rows[0].image_id, image_id);
    }

    #[sqlx::test]
    async fn test_get_images_for_query() {
        let pool = new_db().await;

        let engine_id = get_engine_id(&pool, "Brave").await.unwrap();
        let fetched_at = chrono::Utc::now().naive_utc();
        let query_id = insert_query(&pool, "img-fetch-test", engine_id, fetched_at)
            .await
            .unwrap();

        // Insert two images
        let img1 = insert_image(&pool, "A", "https://a.com").await.unwrap();
        let img2 = insert_image(&pool, "B", "https://b.com").await.unwrap();

        insert_query_image(&pool, query_id, img1, 0).await.unwrap();
        insert_query_image(&pool, query_id, img2, 1).await.unwrap();

        // Fetch images
        let images = get_images_for_query(&pool, query_id).await.unwrap();

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].title, "A");
        assert_eq!(images[1].title, "B");
    }

    #[sqlx::test]
    async fn test_upsert_query_with_images_basic() {
        let pool = new_db().await;

        let entries = vec![
            ImagesRow {
                url: "https://a.com".into(),
                title: "A".into(),
            },
            ImagesRow {
                url: "https://b.com".into(),
                title: "B".into(),
            },
        ];

        let query = "img-upsert";
        let fetched_at = chrono::Utc::now().naive_utc();

        let query_id = upsert_query_with_images(&pool, "Brave", query, entries.clone(), fetched_at)
            .await
            .unwrap();

        assert!(query_id > 0);

        // Fetch back
        let imgs = get_images_for_query(&pool, query_id).await.unwrap();

        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].url, entries[0].url);
        assert_eq!(imgs[1].url, entries[1].url);
    }

    #[sqlx::test]
    async fn test_upsert_query_with_images_append() {
        let pool = new_db().await;

        let page1 = vec![ImagesRow {
            url: "https://a.com".into(),
            title: "A".into(),
        }];

        let page2 = vec![
            ImagesRow {
                url: "https://b.com".into(),
                title: "B".into(),
            },
            ImagesRow {
                url: "https://c.com".into(),
                title: "C".into(),
            },
        ];

        let query = "img-append-test";
        let fetched_at = chrono::Utc::now().naive_utc();

        let id1 = upsert_query_with_images(&pool, "Brave", query, page1.clone(), fetched_at)
            .await
            .unwrap();

        let id2 = upsert_query_with_images(&pool, "Brave", query, page2.clone(), fetched_at)
            .await
            .unwrap();

        // Same query id
        assert_eq!(id1, id2);

        // Should now contain 3 total images, in order
        let imgs = get_images_for_query(&pool, id1).await.unwrap();

        assert_eq!(imgs.len(), 3);
        assert_eq!(imgs[0].title, "A");
        assert_eq!(imgs[1].title, "B");
        assert_eq!(imgs[2].title, "C");
    }
}
