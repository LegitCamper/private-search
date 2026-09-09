//! Low-level SQLite access. Nothing here knows about ranking or engines —
//! just namespaces/queries/rows/orders/progress bookkeeping.

use serde::{Serialize, de::DeserializeOwned};
use sqlx::{Sqlite, SqlitePool, Transaction, sqlite::SqliteConnectOptions};
use std::{env, str::FromStr, time::Duration};

const DEFAULT_SQLITE_DB_NAME: &str = "data/cache.db";
const SQLITE_DB_ENV: &str = "CACHE_DB_PATH";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bumped whenever the schema shape changes. Since this is a pure, disposable,
/// TTL'd cache (never a source of truth), a version mismatch just drops and
/// recreates the cache tables instead of running a data migration.
///
/// v2 separates query membership/attribution (independent of any
/// presentation order) from versioned, persisted `orders`: an append-only
/// `arrival` order built incrementally as engines respond, and immutable
/// `canonical` orders published atomically once a full ranking pass
/// completes. v1's `query_rows` (a single mutable merged index) is gone —
/// there is no compatibility shim for it, since the cache is disposable.
const SCHEMA_VERSION: i64 = 2;

pub async fn init() -> Result<SqlitePool, sqlx::Error> {
    let db_path = env::var(SQLITE_DB_ENV).unwrap_or_else(|_| DEFAULT_SQLITE_DB_NAME.to_string());

    if let Some(parent) = std::path::Path::new(&db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
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
    // Children before parents, though `PRAGMA foreign_keys` doesn't enforce
    // ordering on DROP — this is just for readability.
    sqlx::query(
        r#"
        DROP TABLE IF EXISTS order_entries;
        DROP TABLE IF EXISTS orders;
        DROP TABLE IF EXISTS query_row_engines;
        DROP TABLE IF EXISTS query_row_members;
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

        -- `canonical_order_id` is the query's single, atomically-switched
        -- pointer at the current immutable canonical order. It's nullable
        -- (no canonical order exists until a build's first ranking pass
        -- completes) and forward-references `orders`, which in turn
        -- references `queries` — SQLite doesn't require the referenced
        -- table to exist yet at CREATE TABLE time, only by the time a
        -- foreign-key-checked statement actually runs, so this circular
        -- pair is fine as long as both tables exist before any DML.
        CREATE TABLE IF NOT EXISTS queries (
            id INTEGER PRIMARY KEY,
            query TEXT NOT NULL,
            namespace_id INTEGER NOT NULL REFERENCES namespaces(id),
            fetched_at DATETIME NOT NULL,
            canonical_order_id INTEGER REFERENCES orders(id),
            UNIQUE (query, namespace_id)
        );

        -- Per-(query, engine) raw pagination progress. Decoupled from
        -- presentation order on purpose: an engine's own offset into its
        -- result stream has nothing to do with any merged/ranked position.
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

        -- Query membership: is `row_id` part of `query_id`'s result set at
        -- all? Completely independent of *where* it sits in any
        -- presentation order — that's `order_entries`' job.
        CREATE TABLE IF NOT EXISTS query_row_members (
            query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
            row_id INTEGER NOT NULL REFERENCES rows(id),
            PRIMARY KEY (query_id, row_id)
        );

        -- Accumulates engine attribution: a URL first surfaced by one engine
        -- and later rediscovered by another still ends up attributed to
        -- both. Keyed off membership (not directly off `queries`/`rows`) so
        -- it cascades when membership disappears, closing the gap in v1
        -- where this table had no cascading FKs of its own at all.
        CREATE TABLE IF NOT EXISTS query_row_engines (
            query_id INTEGER NOT NULL,
            row_id INTEGER NOT NULL,
            engine_id INTEGER NOT NULL REFERENCES engines(id),
            PRIMARY KEY (query_id, row_id, engine_id),
            FOREIGN KEY (query_id, row_id)
                REFERENCES query_row_members (query_id, row_id) ON DELETE CASCADE
        );

        -- A persisted, versioned presentation order over a query's
        -- membership. `arrival` orders are appended to incrementally, in
        -- engine-completion order, while a build is in progress; `canonical`
        -- orders are written once, atomically, by ranking the complete
        -- member set, and are never mutated again. Old orders (of either
        -- kind) are kept until the owning query itself is purged, so a
        -- client paginating against a specific order token keeps a stable
        -- position space even after a newer canonical order is published.
        CREATE TABLE IF NOT EXISTS orders (
            id INTEGER PRIMARY KEY,
            query_id INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
            kind TEXT NOT NULL CHECK (kind IN ('arrival', 'canonical')),
            created_at DATETIME NOT NULL
        );

        CREATE TABLE IF NOT EXISTS order_entries (
            order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
            row_id INTEGER NOT NULL REFERENCES rows(id),
            position INTEGER NOT NULL,
            PRIMARY KEY (order_id, position),
            UNIQUE (order_id, row_id)
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

/// `INSERT OR IGNORE` + fallback `SELECT` (rather than `SELECT` then
/// `INSERT`) so two connections racing to create the same brand-new query
/// can't both observe "doesn't exist yet" and then both try to insert it —
/// the loser's insert is silently ignored by SQLite instead of erroring on
/// the `UNIQUE (query, namespace_id)` constraint, and it just re-selects the
/// winner's row.
pub(crate) async fn get_or_create_query(
    tx: &mut Transaction<'_, Sqlite>,
    query: &str,
    namespace_id: i64,
    fetched_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO queries (query, namespace_id, fetched_at) VALUES (?, ?, ?)",
    )
    .bind(query)
    .bind(namespace_id)
    .bind(fetched_at)
    .execute(&mut **tx)
    .await?;

    if res.rows_affected() == 0 {
        let (id,): (i64,) =
            sqlx::query_as("SELECT id FROM queries WHERE query = ? AND namespace_id = ?")
                .bind(query)
                .bind(namespace_id)
                .fetch_one(&mut **tx)
                .await?;
        Ok(id)
    } else {
        Ok(res.last_insert_rowid())
    }
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

/// Inserts (or reuses) the global `rows` entry for `url`, returning its id.
pub(crate) async fn get_or_create_row<R: Serialize>(
    tx: &mut Transaction<'_, Sqlite>,
    url: &str,
    value: &R,
) -> Result<i64, sqlx::Error> {
    let payload = serde_json::to_string(value).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

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

/// Marks `row_id` as part of `query_id`'s member set. Idempotent — safe to
/// call for a row that's already a member (e.g. a rediscovered URL).
pub(crate) async fn add_member(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    row_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO query_row_members (query_id, row_id) VALUES (?, ?)")
        .bind(query_id)
        .bind(row_id)
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

/// Every engine that has ever attributed to `row_id` for `query_id`, sorted
/// by name for deterministic output.
pub(crate) async fn engines_for_row(
    pool: &SqlitePool,
    query_id: i64,
    row_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT e.name
        FROM query_row_engines qre
        JOIN engines e ON e.id = qre.engine_id
        WHERE qre.query_id = ? AND qre.row_id = ?
        ORDER BY e.name ASC
        "#,
    )
    .bind(query_id)
    .bind(row_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// Which immutable/append-only bucket an [`orders`] row belongs to. See the
/// `orders` table comment in [`create_schema`] for the semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// Built incrementally, in engine-completion order, while a build is
    /// in progress. Safe to keep appending to.
    Arrival,
    /// Written once from a full ranking pass over the complete member set,
    /// then never mutated again.
    Canonical,
}

impl OrderKind {
    fn as_str(self) -> &'static str {
        match self {
            OrderKind::Arrival => "arrival",
            OrderKind::Canonical => "canonical",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "canonical" => OrderKind::Canonical,
            _ => OrderKind::Arrival,
        }
    }
}

/// Creates a brand-new, empty order of `kind` for `query_id`.
pub(crate) async fn create_order(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    kind: OrderKind,
    created_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    Ok(
        sqlx::query("INSERT INTO orders (query_id, kind, created_at) VALUES (?, ?, ?)")
            .bind(query_id)
            .bind(kind.as_str())
            .bind(created_at)
            .execute(&mut **tx)
            .await?
            .last_insert_rowid(),
    )
}

/// Forks `source_order_id` into a brand-new, appendable `arrival` order that
/// starts with an exact copy of the source's entries (same row, same
/// position). Used when an immutable canonical order needs to be extended:
/// rather than mutating it, extension continues on this fork, so a client
/// already paginating the canonical order's token is unaffected.
pub(crate) async fn fork_order(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    source_order_id: i64,
    created_at: chrono::NaiveDateTime,
) -> Result<i64, sqlx::Error> {
    let new_order_id = create_order(tx, query_id, OrderKind::Arrival, created_at).await?;
    sqlx::query(
        r#"
        INSERT INTO order_entries (order_id, row_id, position)
        SELECT ?, row_id, position FROM order_entries WHERE order_id = ?
        "#,
    )
    .bind(new_order_id)
    .bind(source_order_id)
    .execute(&mut **tx)
    .await?;
    Ok(new_order_id)
}

/// Appends one committed row to `order_id` at `position`. Callers own
/// position assignment (always `current length + offset`) since only they
/// know how many rows they're committing in this batch.
pub(crate) async fn append_order_entry(
    tx: &mut Transaction<'_, Sqlite>,
    order_id: i64,
    row_id: i64,
    position: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO order_entries (order_id, row_id, position) VALUES (?, ?, ?)")
        .bind(order_id)
        .bind(row_id)
        .bind(position)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Number of entries in `order_id` — since positions are assigned densely
/// from 0, this doubles as "one past the highest position", i.e. the next
/// append position.
pub(crate) async fn order_len(pool: &SqlitePool, order_id: i64) -> Result<i64, sqlx::Error> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM order_entries WHERE order_id = ?")
        .bind(order_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// The query's current canonical order id, if any build has ever completed
/// a ranking pass for it.
pub(crate) async fn get_canonical_order_id(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let (id,): (Option<i64>,) =
        sqlx::query_as("SELECT canonical_order_id FROM queries WHERE id = ?")
            .bind(query_id)
            .fetch_one(pool)
            .await?;
    Ok(id)
}

/// The kind of `order_id`, provided it actually belongs to `query_id` — a
/// token from a different query (or a stale/bogus one) resolves to `None`
/// rather than leaking cross-query state.
pub(crate) async fn get_order_kind(
    pool: &SqlitePool,
    order_id: i64,
    query_id: i64,
) -> Result<Option<OrderKind>, sqlx::Error> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT kind FROM orders WHERE id = ? AND query_id = ?")
            .bind(order_id)
            .bind(query_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(k,)| OrderKind::parse(&k)))
}

/// Atomically switches `query_id`'s canonical pointer to `order_id`. Called
/// in the same transaction that finished writing `order_id`'s entries, so a
/// crash never leaves the pointer aimed at a half-written order.
pub(crate) async fn set_canonical_order(
    tx: &mut Transaction<'_, Sqlite>,
    query_id: i64,
    order_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE queries SET canonical_order_id = ? WHERE id = ?")
        .bind(order_id)
        .bind(query_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// One row of a loaded order window or full member set, with enough
/// identity (`row_id`, `url`) to attribute newly-discovered engine hits
/// against it later, plus every engine that has ever surfaced it.
pub(crate) struct MemberRow<R> {
    pub row_id: i64,
    pub url: String,
    pub value: R,
    pub engines: Vec<String>,
}

/// One row of a specific order, with its position in that order.
pub(crate) struct OrderedRow<R> {
    pub position: i64,
    pub row: MemberRow<R>,
}

/// Loads `[start, start+count)` of `order_id`'s entries (joined against the
/// live `query_id` attribution — attribution is per-query, not per-order),
/// for serving an already-covered window straight from the cache.
pub(crate) async fn get_order_window<R: DeserializeOwned>(
    pool: &SqlitePool,
    order_id: i64,
    query_id: i64,
    start: usize,
    count: usize,
) -> Result<Vec<OrderedRow<R>>, sqlx::Error> {
    let raw: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT oe.position, r.id, r.url, r.payload, e.name
        FROM order_entries oe
        JOIN rows r ON r.id = oe.row_id
        LEFT JOIN query_row_engines qre ON qre.query_id = ? AND qre.row_id = oe.row_id
        LEFT JOIN engines e ON e.id = qre.engine_id
        WHERE oe.order_id = ? AND oe.position >= ? AND oe.position < ?
        ORDER BY oe.position ASC, e.name ASC
        "#,
    )
    .bind(query_id)
    .bind(order_id)
    .bind(start as i64)
    .bind((start + count) as i64)
    .fetch_all(pool)
    .await?;

    group_ordered_rows(raw)
}

fn group_ordered_rows<R: DeserializeOwned>(
    raw: Vec<(i64, i64, String, String, Option<String>)>,
) -> Result<Vec<OrderedRow<R>>, sqlx::Error> {
    let mut out: Vec<OrderedRow<R>> = Vec::new();
    for (position, row_id, url, payload, engine_name) in raw {
        match out.last_mut() {
            Some(last) if last.row.row_id == row_id && last.position == position => {
                if let Some(name) = engine_name {
                    last.row.engines.push(name);
                }
            }
            _ => {
                let value: R =
                    serde_json::from_str(&payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                out.push(OrderedRow {
                    position,
                    row: MemberRow {
                        row_id,
                        url,
                        value,
                        engines: engine_name.into_iter().collect(),
                    },
                });
            }
        }
    }
    Ok(out)
}

/// Loads every row that is a member of `query_id`, regardless of any
/// order, with full attribution — used for the final ranking pass that
/// produces a fresh canonical order. Ordered by `row_id` purely for
/// deterministic input to the ranker on ties; the ranker is expected to
/// impose its own real order.
pub(crate) async fn get_all_members<R: DeserializeOwned>(
    pool: &SqlitePool,
    query_id: i64,
) -> Result<Vec<MemberRow<R>>, sqlx::Error> {
    let raw: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT r.id, r.url, r.payload, e.name
        FROM query_row_members qrm
        JOIN rows r ON r.id = qrm.row_id
        LEFT JOIN query_row_engines qre ON qre.query_id = qrm.query_id AND qre.row_id = qrm.row_id
        LEFT JOIN engines e ON e.id = qre.engine_id
        WHERE qrm.query_id = ?
        ORDER BY r.id ASC, e.name ASC
        "#,
    )
    .bind(query_id)
    .fetch_all(pool)
    .await?;

    let mut out: Vec<MemberRow<R>> = Vec::new();
    for (row_id, url, payload, engine_name) in raw {
        match out.last_mut() {
            Some(last) if last.row_id == row_id => {
                if let Some(name) = engine_name {
                    last.engines.push(name);
                }
            }
            _ => {
                let value: R =
                    serde_json::from_str(&payload).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                out.push(MemberRow {
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

/// One freshly-fetched, not-yet-persisted row plus the URL used for
/// dedup/lookup (kept alongside rather than re-derived from `value` so this
/// module doesn't need to know about `CacheableRow`).
pub(crate) struct FreshRow<R> {
    pub url: String,
    pub value: R,
}

/// What actually got persisted by one engine's settlement.
pub(crate) struct RoundCommit<R> {
    /// `(row_id, position, original fresh row)`, in the same order they were
    /// appended.
    pub fresh: Vec<(i64, i64, FreshRow<R>)>,
    /// `(row_id, url, full current engine list)` for every URL this engine
    /// rediscovered rather than introduced.
    pub rediscovered_engines: Vec<(i64, String, Vec<String>)>,
}

/// Everything one engine's settlement needs persisted, bundled into a struct
/// rather than passed positionally since it's a lot of related state.
pub(crate) struct EngineRoundCommit<R> {
    pub query_id: i64,
    pub order_id: i64,
    pub start_position: i64,
    pub engine_name: &'static str,
    pub next_start: i64,
    pub exhausted: bool,
    pub fetched_at: chrono::NaiveDateTime,
    /// Already ranked and in final relative order.
    pub fresh: Vec<FreshRow<R>>,
    pub rediscovered: Vec<(i64, String)>,
}

/// Persists one engine's contribution to one round in a single transaction:
/// touches the query, updates that engine's progress, attributes rediscovered
/// rows, and appends freshly-discovered rows to `order_id` starting at
/// `start_position`. Nothing here is visible to any reader until it commits,
/// and the caller is expected to only emit stream events afterward.
pub(crate) async fn commit_engine_round<R: Serialize>(
    pool: &SqlitePool,
    params: EngineRoundCommit<R>,
) -> Result<RoundCommit<R>, sqlx::Error> {
    let EngineRoundCommit {
        query_id,
        order_id,
        start_position,
        engine_name,
        next_start,
        exhausted,
        fetched_at,
        fresh,
        rediscovered,
    } = params;

    let mut tx = pool.begin().await?;
    touch_query(&mut tx, query_id, fetched_at).await?;
    set_progress(&mut tx, query_id, engine_name, next_start, exhausted).await?;

    for (row_id, _) in &rediscovered {
        attribute_engine(&mut tx, query_id, *row_id, engine_name).await?;
    }

    let mut committed = Vec::with_capacity(fresh.len());
    for (offset, item) in fresh.into_iter().enumerate() {
        let position = start_position + offset as i64;
        let row_id = get_or_create_row(&mut tx, &item.url, &item.value).await?;
        add_member(&mut tx, query_id, row_id).await?;
        append_order_entry(&mut tx, order_id, row_id, position).await?;
        attribute_engine(&mut tx, query_id, row_id, engine_name).await?;
        committed.push((row_id, position, item));
    }

    tx.commit().await?;

    // Read back post-commit, outside the transaction — nothing else writes
    // this order/query concurrently (the shared-build registry guarantees a
    // single owner), so a fresh read here is safe and keeps the write
    // transaction itself minimal.
    let mut rediscovered_engines = Vec::with_capacity(rediscovered.len());
    for (row_id, url) in rediscovered {
        let engines = engines_for_row(pool, query_id, row_id).await?;
        rediscovered_engines.push((row_id, url, engines));
    }

    Ok(RoundCommit {
        fresh: committed,
        rediscovered_engines,
    })
}

pub async fn purge_stale_queries(
    pool: &SqlitePool,
    cutoff: chrono::NaiveDateTime,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // `queries` cascades to `query_engine_progress`, `query_row_members`
    // (which itself cascades to `query_row_engines`), and `orders` (which
    // cascades to `order_entries`) — see the FKs in `create_schema`.
    let purged = sqlx::query("DELETE FROM queries WHERE fetched_at < ?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query("DELETE FROM rows WHERE id NOT IN (SELECT row_id FROM query_row_members)")
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(purged)
}
