//! Low-level SQLite access. Nothing here knows about ranking or engines —
//! just namespaces/queries/rows/progress bookkeeping.

use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Sqlite, SqlitePool, Transaction, sqlite::SqliteConnectOptions};
use std::{env, str::FromStr, time::Duration};

const DEFAULT_SQLITE_DB_NAME: &str = "data/cache.db";
const SQLITE_DB_ENV: &str = "CACHE_DB_PATH";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bumped whenever the schema shape changes. Since this is a pure, disposable,
/// TTL'd cache (never a source of truth), a version mismatch just drops and
/// recreates the cache tables instead of running a data migration.
const SCHEMA_VERSION: i64 = 1;

pub async fn init() -> Result<SqlitePool, sqlx::Error> {
    let db_path = env::var(SQLITE_DB_ENV).unwrap_or_else(|_| DEFAULT_SQLITE_DB_NAME.to_string());

    if let Some(parent) = std::path::Path::new(&db_path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).expect("failed to create cache db directory");
    }

    let options = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))
        .expect("invalid CACHE_DB_PATH")
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .expect("FAILED TO CONNECT TO DB");

    ensure_schema(&pool).await.expect("FAILED TO INITIALIZE DB");

    Ok(pool)
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;

    if version != SCHEMA_VERSION {
        drop_schema(pool).await?;
        create_schema(pool).await?;
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(pool)
            .await?;
    }

    Ok(())
}

async fn drop_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        DROP TABLE IF EXISTS query_row_engines;
        DROP TABLE IF EXISTS query_rows;
        DROP TABLE IF EXISTS rows;
        DROP TABLE IF EXISTS query_engine_progress;
        DROP TABLE IF EXISTS queries;
        DROP TABLE IF EXISTS namespaces;
        DROP TABLE IF EXISTS engines;
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn create_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS namespaces (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS engines (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS queries (
            id INTEGER PRIMARY KEY,
            query TEXT NOT NULL,
            namespace_id INTEGER NOT NULL REFERENCES namespaces(id),
            fetched_at DATETIME NOT NULL,
            UNIQUE (query, namespace_id)
        );

        -- Per-(query, engine) raw pagination progress. Decoupled from the
        -- client-visible merged index in `query_rows` on purpose: an engine's
        -- own offset into its result stream has nothing to do with how many
        -- deduped/ranked rows a client has been shown.
        CREATE TABLE IF NOT EXISTS query_engine_progress (
            query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
            engine_id INTEGER NOT NULL REFERENCES engines(id),
            next_start INTEGER NOT NULL DEFAULT 0,
            exhausted INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (query_id, engine_id)
        );

        -- One row per unique URL, globally reused across every query that
        -- surfaces it. `payload` is the caller's row type, serialized —
        -- this crate has no idea what shape it is.
        CREATE TABLE IF NOT EXISTS rows (
            id INTEGER PRIMARY KEY,
            url TEXT NOT NULL UNIQUE,
            payload TEXT NOT NULL
        );

        -- The stable, append-only merged order for a query. `merged_index`
        -- IS the index space a client's `start`/`count` addresses — once
        -- assigned it never changes, so pagination never drifts.
        CREATE TABLE IF NOT EXISTS query_rows (
            query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
            row_id INTEGER NOT NULL REFERENCES rows(id),
            merged_index INTEGER NOT NULL,
            PRIMARY KEY (query_id, row_id),
            UNIQUE (query_id, merged_index)
        );

        -- Accumulates engine attribution: a URL first surfaced by one engine
        -- and later rediscovered by another still ends up attributed to both.
        CREATE TABLE IF NOT EXISTS query_row_engines (
            query_id INTEGER NOT NULL,
            row_id INTEGER NOT NULL,
            engine_id INTEGER NOT NULL REFERENCES engines(id),
            PRIMARY KEY (query_id, row_id, engine_id)
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub(crate) async fn get_or_create_namespace(
    tx: &mut Transaction<'_, Sqlite>,
    name: &str,
) -> Result<i64, sqlx::Error> {
    if let Some((id,)) = sqlx::query_as::<_, (i64,)>("SELECT id FROM namespaces WHERE name = ?")
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query("INSERT INTO namespaces (name) VALUES (?)")
        .bind(name)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid())
}

pub(crate) async fn get_or_create_engine(
    tx: &mut Transaction<'_, Sqlite>,
    name: &str,
) -> Result<i64, sqlx::Error> {
    if let Some((id,)) = sqlx::query_as::<_, (i64,)>("SELECT id FROM engines WHERE name = ?")
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query("INSERT INTO engines (name) VALUES (?)")
        .bind(name)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid())
}

pub(crate) async fn get_or_create_query(
    tx: &mut Transaction<'_, Sqlite>,
    query: &str,
    namespace_id: i64,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    if let Some((id,)) =
        sqlx::query_as::<_, (i64,)>("SELECT id FROM queries WHERE query = ? AND namespace_id = ?")
            .bind(query)
            .bind(namespace_id)
            .fetch_optional(&mut **tx)
            .await?
    {
        return Ok(id);
    }
    Ok(
        sqlx::query("INSERT INTO queries (query, namespace_id, fetched_at) VALUES (?, ?, ?)")
            .bind(query)
            .bind(namespace_id)
            .bind(fetched_at)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid(),
    )
}

pub(crate) async fn touch_query(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    fetched_at: chrono::NaiveDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE queries SET fetched_at = ? WHERE id = ?")
        .bind(fetched_at)
        .bind(query_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// `(next_start, exhausted)`, defaulting to `(0, false)` if this engine has
/// never been queried for this query yet.
pub(crate) async fn get_progress(
    pool: &SqlitePool,
    query_id: i64,
    engine_name: &str,
) -> Result<(i64, bool), sqlx::Error> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        r#"
        SELECT p.next_start, p.exhausted
        FROM query_engine_progress p
        JOIN engines e ON e.id = p.engine_id
        WHERE p.query_id = ? AND e.name = ?
        "#,
    )
    .bind(query_id)
    .bind(engine_name)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(n, e)| (n, e != 0)).unwrap_or((0, false)))
}

pub(crate) async fn set_progress(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    engine_name: &str,
    next_start: i64,
    exhausted: bool,
) -> Result<(), sqlx::Error> {
    let engine_id = get_or_create_engine(tx, engine_name).await?;
    sqlx::query(
        r#"
        INSERT INTO query_engine_progress (query_id, engine_id, next_start, exhausted)
        VALUES (?, ?, ?, ?)
        ON CONFLICT (query_id, engine_id)
        DO UPDATE SET next_start = excluded.next_start, exhausted = excluded.exhausted
        "#,
    )
    .bind(query_id)
    .bind(engine_id)
    .bind(next_start)
    .bind(exhausted)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// One row of the persisted merged list, with enough identity (`row_id`,
/// `url`) to attribute newly-discovered engine hits against it later.
pub(crate) struct MergedRow<R> {
    pub row_id: i64,
    pub url: String,
    pub value: R,
    pub engines: Vec<String>,
}

pub(crate) async fn get_merged_rows<R: DeserializeOwned>(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<MergedRow<R>>, sqlx::Error> {
    let raw: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT qr.merged_index, r.id, r.url, r.payload, e.name
        FROM query_rows qr
        JOIN rows r ON r.id = qr.row_id
        LEFT JOIN query_row_engines qre ON qre.query_id = qr.query_id AND qre.row_id = qr.row_id
        LEFT JOIN engines e ON e.id = qre.engine_id
        WHERE qr.query_id = ?
        ORDER BY qr.merged_index ASC, e.name ASC
        "#,
    )
    .bind(query_id)
    .fetch_all(pool)
    .await?;

    let mut out: Vec<MergedRow<R>> = Vec::new();
    for (_merged_index, row_id, url, payload, engine_name) in raw {
        match out.last_mut() {
            Some(last) if last.row_id == row_id => {
                if let Some(name) = engine_name {
                    last.engines.push(name);
                }
            }
            _ => {
                let value: R = serde_json::from_str(&payload)
                    .expect("cached payload didn't deserialize as the expected row type");
                out.push(MergedRow {
                    row_id,
                    url,
                    value,
                    engines: engine_name.into_iter().collect(),
                });
            }
        }
    }

    Ok(out)
}

/// Inserts (or reuses) the global `rows` entry for `url`, returning its id.
pub(crate) async fn get_or_create_row<R: Serialize>(
    tx: &mut Transaction<'_, Sqlite>,
    url: &str,
    value: &R,
) -> Result<i64, sqlx::Error> {
    let payload = serde_json::to_string(value).expect("row type must be serializable");

    let res = sqlx::query("INSERT OR IGNORE INTO rows (url, payload) VALUES (?, ?)")
        .bind(url)
        .bind(&payload)
        .execute(&mut **tx)
        .await?;

    if res.rows_affected() == 0 {
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM rows WHERE url = ?")
            .bind(url)
            .fetch_one(&mut **tx)
            .await?;
        Ok(id)
    } else {
        Ok(res.last_insert_rowid())
    }
}

pub(crate) async fn link_query_row(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    row_id: i64,
    merged_index: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO query_rows (query_id, row_id, merged_index) VALUES (?, ?, ?)")
        .bind(query_id)
        .bind(row_id)
        .bind(merged_index)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn attribute_engine(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    row_id: i64,
    engine_name: &str,
) -> Result<(), sqlx::Error> {
    let engine_id = get_or_create_engine(tx, engine_name).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO query_row_engines (query_id, row_id, engine_id) VALUES (?, ?, ?)",
    )
    .bind(query_id)
    .bind(row_id)
    .bind(engine_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn purge_stale_queries(
    pool: &SqlitePool,
    cutoff: chrono::NaiveDateTime,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let purged = sqlx::query("DELETE FROM queries WHERE fetched_at < ?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query("DELETE FROM rows WHERE id NOT IN (SELECT row_id FROM query_rows)")
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(purged)
}
