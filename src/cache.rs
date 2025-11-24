use sqlx::{SqlitePool, prelude::FromRow};
use strum::IntoEnumIterator;

use crate::engines::{EngineResult, Engines};

const SQLITE_DB_NAME: &'static str = "cache.db";

pub async fn init() -> Result<SqlitePool, sqlx::Error> {
    let conn = SqlitePool::connect(SQLITE_DB_NAME)
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
        "#,
    )
    .execute(conn)
    .await?;

    for engine in Engines::iter() {
        insert_engine(conn, engine).await?;
    }

    Ok(())
}

pub async fn upsert_query_with_results(
    pool: &SqlitePool,
    engine: Engines,
    query: &str,
    entries: Vec<EngineResult>,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let engine_id = get_engine_id(pool, engine).await?;
    let query_row = get_query(pool, query, engine_id).await?;

    // start sql transaction
    let tx = pool.begin().await?;

    let query_id = if let Some(q) = query_row {
        q.id
    } else {
        insert_query(pool, query, engine_id, fetched_at).await?
    };

    // Determine starting index for new results
    let current_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM query_results WHERE query_id = ?")
            .bind(query_id)
            .fetch_one(pool)
            .await?;

    for (i, entry) in entries.iter().enumerate() {
        let result_id = insert_result(pool, &entry.title, &entry.url, &entry.description).await?;
        insert_query_result(pool, query_id, result_id, current_count + i as i64).await?;
    }

    tx.commit().await?;

    Ok(query_id)
}

#[derive(FromRow)]
pub struct EngineRow {
    pub id: i64,
    pub name: Engines,
}

pub async fn get_engine_id(pool: &SqlitePool, engine: Engines) -> Result<i64, sqlx::Error> {
    let row: EngineRow = sqlx::query_as("SELECT id, name FROM engines WHERE name = ?")
        .bind(engine)
        .fetch_one(pool)
        .await?;

    Ok(row.id)
}

pub async fn insert_engine(pool: &SqlitePool, engine: Engines) -> Result<i64, sqlx::Error> {
    let id = sqlx::query("INSERT OR IGNORE INTO engines (name) VALUES (?)")
        .bind(engine)
        .execute(pool)
        .await?
        .last_insert_rowid();

    Ok(id)
}

#[derive(FromRow)]
pub struct QueryRow {
    pub id: i64,
    pub query: String,
    pub engine_id: i64,
    pub fetched_at: chrono::NaiveDateTime,
}

pub async fn get_query(
    pool: &SqlitePool,
    query: &str,
    engine_id: i64,
) -> Result<Option<QueryRow>, sqlx::Error> {
    let row: Option<QueryRow> = sqlx::query_as(
        r#"
        SELECT id, query, engine_id, fetched_at
        FROM queries
        WHERE query = ? AND engine_id = ?
        "#,
    )
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
    let id = sqlx::query(
        r#"
        INSERT INTO queries (query, engine_id, fetched_at)
        VALUES (?, ?, ?)
        "#,
    )
    .bind(query)
    .bind(engine_id)
    .bind(fetched_at)
    .execute(pool)
    .await?
    .last_insert_rowid();

    Ok(id)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EngineResultRow {
    pub url: String,
    pub title: String,
    pub description: String,
}

impl Into<EngineResult> for EngineResultRow {
    fn into(self) -> EngineResult {
        EngineResult {
            url: self.url,
            title: self.title,
            description: self.description,
        }
    }
}

pub async fn get_results_for_query(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<EngineResultRow>, sqlx::Error> {
    let rows: Vec<EngineResultRow> = sqlx::query_as(
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

pub async fn insert_result(
    pool: &SqlitePool,
    title: &str,
    url: &str,
    description: &str,
) -> Result<i64, sqlx::Error> {
    let res =
        sqlx::query("INSERT OR IGNORE INTO results (url, title, description) VALUES (?, ?, ?)")
            .bind(url)
            .bind(title)
            .bind(description)
            .execute(pool)
            .await?;

    if res.rows_affected() == 0 {
        // Already exists - fetch id
        let row: (i64,) = sqlx::query_as("SELECT id FROM results WHERE url = ?")
            .bind(url)
            .fetch_one(pool)
            .await?;
        Ok(row.0)
    } else {
        Ok(res.last_insert_rowid())
    }
}

#[derive(sqlx::FromRow)]
pub struct QueryResultRow {
    pub query_id: i64,
    pub result_id: i64,
}

pub async fn insert_query_result(
    pool: &SqlitePool,
    query_id: i64,
    result_id: i64,
    result_index: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO query_results (query_id, result_id, result_index)
        VALUES (?, ?, ?)
        "#,
    )
    .bind(query_id)
    .bind(result_id)
    .bind(result_index)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_result_for_query(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<QueryResultRow>, sqlx::Error> {
    Ok(
        sqlx::query_as("SELECT query_id, result_id FROM query_results WHERE query_id = ?")
            .bind(query_id)
            .fetch_all(pool)
            .await?,
    )
}

#[cfg(test)]
mod test {
    use crate::{
        cache::{create_search_cache, get_results_for_query, upsert_query_with_results},
        engines::{EngineResult, Engines},
    };
    use chrono::Utc;
    use sqlx::SqlitePool;

    async fn new_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();

        create_search_cache(&pool).await.unwrap();

        pool
    }

    fn sample_results() -> Vec<EngineResult> {
        vec![
            EngineResult {
                url: "https://example.com".into(),
                title: "Example 1".into(),
                description: "First description".into(),
            },
            EngineResult {
                url: "https://super.com".into(),
                title: "Example 2".into(),
                description: "Second description".into(),
            },
            EngineResult {
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
            upsert_query_with_results(&pool, Engines::Google, query, results.clone(), fetched_at)
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
            upsert_query_with_results(&pool, Engines::Bing, query, results.clone(), fetched_at)
                .await
                .unwrap();

        // second insert with same query/results
        let second_id =
            upsert_query_with_results(&pool, Engines::Bing, query, results.clone(), fetched_at)
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
            EngineResult {
                url: "https://extra.com".into(),
                title: "Extra 1".into(),
                description: "Extra description".into(),
            },
            EngineResult {
                url: "https://more.com".into(),
                title: "Extra 2".into(),
                description: "More description".into(),
            },
        ];

        let query = "pagination test";
        let fetched_at = Utc::now().naive_utc();

        // Insert page 1
        let query_id =
            upsert_query_with_results(&pool, Engines::Google, query, page1.clone(), fetched_at)
                .await
                .unwrap();

        // Append page 2
        upsert_query_with_results(&pool, Engines::Google, query, page2.clone(), fetched_at)
            .await
            .unwrap();

        // Verify all results
        let fetched = get_results_for_query(&pool, query_id).await.unwrap();
        assert_eq!(fetched.len(), page1.len() + page2.len());
        assert_eq!(fetched[0].url, page1[0].url);
        assert_eq!(fetched.last().unwrap().url, page2.last().unwrap().url);
    }
}
