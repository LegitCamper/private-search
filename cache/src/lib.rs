//! A generic, ranked, append-only merge cache for paginated multi-source
//! search results, backed by SQLite.
//!
//! This crate has no idea what a "search engine" or a "ranking algorithm"
//! is — callers bring their own row type ([`CacheableRow`]), their own
//! ordering policy ([`Ranker`]), and their own way of pulling one more page
//! from a source ([`EngineSource`]). What this crate guarantees in return:
//! results for a query are deduplicated and stored in one canonical,
//! **stable, append-only** order, so a client's `start`/`count` is always a
//! true index into that order — no drift between a cold fetch and a cache
//! hit, and no dropped/duplicated pages during pagination.

mod db;

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, OnceCell},
    task::JoinSet,
    time::timeout,
};

pub use db::init;

/// Caps rounds of "fetch more, still not enough" per call, so a deep `start`
/// or a source with broken pagination can't loop forever.
const MAX_ROUNDS: usize = 10;

/// A row that can live in the merge cache. `url` is the identity used for
/// deduplication (including across different sources/rounds).
pub trait CacheableRow: Clone + Send + Sync + Serialize + DeserializeOwned + 'static {
    fn url(&self) -> &str;
}

/// Orders one freshly-fetched, already-deduplicated batch. The cache handles
/// dedup/persistence/pagination itself; a `Ranker` only decides relative
/// order *within* a batch — rows already persisted are never reordered, so
/// ranking only ever affects where newly-discovered rows land at the tail.
pub trait Ranker<R: CacheableRow>: Send + Sync {
    fn rank(&self, query: &str, batch: Vec<R>) -> Vec<R>;
}

/// Caller-supplied way to pull one more raw page from one upstream source
/// (e.g. a search engine). Decouples this crate from knowing about any
/// particular engine. `start` is that source's own raw offset — callers get
/// it back via [`ExtendResult`]/progress tracking, never derived by the
/// client.
#[async_trait]
pub trait EngineSource<R: CacheableRow>: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<R>, String>;
}

#[derive(Debug)]
pub enum CacheError {
    Sqlx(sqlx::Error),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Sqlx(e) => write!(f, "cache db error: {e}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<sqlx::Error> for CacheError {
    fn from(e: sqlx::Error) -> Self {
        CacheError::Sqlx(e)
    }
}

/// One row of an [`ExtendResult`], with the engines that contributed it
/// (accumulated across every round/source that has ever surfaced its URL for
/// this query) and whether it was already cached before this call started.
#[derive(Debug, Clone)]
pub struct MergedRowResult<R> {
    pub value: R,
    pub engines: Vec<String>,
    pub cached: bool,
}

/// How one source fared on a single `get_or_extend` call. Timeouts are their
/// own variant (rather than folded into `Failed`) because the timeout itself
/// is this crate's own `round_timeout`, not something the source reported.
#[derive(Debug, Clone)]
pub enum EngineOutcome {
    Ok,
    Failed(String),
    TimedOut,
}

pub struct ExtendResult<R> {
    pub rows: Vec<MergedRowResult<R>>,
    /// True if there are more merged rows beyond this slice, or at least one
    /// requested source hasn't yet proven itself exhausted — a real
    /// exhaustion signal, not a "did this page look full" heuristic.
    pub has_more: bool,
    /// Per-source outcome for whichever sources were actually contacted this
    /// call (sources fully served from cache won't appear here).
    pub engine_outcomes: Vec<(String, EngineOutcome)>,
}

/// Per-`(namespace, query)` locks so concurrent requests for the same query
/// (duplicate/overlapping polls from a client) don't both miss the cache and
/// fire off redundant source requests + concurrent SQLite writes.
static QUERY_LOCKS: StdMutex<Option<HashMap<String, Arc<AsyncMutex<()>>>>> = StdMutex::new(None);

async fn lock_for_query(key: String) -> Arc<AsyncMutex<()>> {
    let mut guard = QUERY_LOCKS.lock().unwrap();
    guard
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn release_query_lock(key: &str, lock: Arc<AsyncMutex<()>>) {
    let mut guard = QUERY_LOCKS.lock().unwrap();
    if let Some(map) = guard.as_mut() {
        // 2 = our local `lock` clone + the one held in the map.
        if Arc::strong_count(&lock) == 2 {
            map.remove(key);
        }
    }
}

static SQLPOOL: OnceCell<SqlitePool> = OnceCell::const_new();

/// Lazily-initialized process-global pool, matching this crate's `init()`.
/// Most consumers should build their own [`SqlitePool`] and pass it to
/// [`MergedCache::new`]; this is a convenience for single-process binaries.
pub async fn shared_pool() -> &'static SqlitePool {
    SQLPOOL
        .get_or_init(|| async { init().await.expect("failed to init cache db") })
        .await
}

pub async fn clean_cache(pool: &SqlitePool, max_age: Duration) -> Result<u64, CacheError> {
    let max_age = chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
    let cutoff = chrono::Utc::now().naive_utc() - max_age;
    Ok(db::purge_stale_queries(pool, cutoff).await?)
}

pub struct MergedCache<R: CacheableRow> {
    pool: SqlitePool,
    namespace: &'static str,
    ranker: Arc<dyn Ranker<R>>,
}

impl<R: CacheableRow> MergedCache<R> {
    pub fn new(pool: SqlitePool, namespace: &'static str, ranker: Arc<dyn Ranker<R>>) -> Self {
        Self {
            pool,
            namespace,
            ranker,
        }
    }

    /// Returns rows `[start, start+count)` for `query`, extending the merged
    /// cache from `sources` (each resumed from its own persisted progress)
    /// until the window is satisfied or every source is exhausted.
    pub async fn get_or_extend(
        &self,
        query: &str,
        sources: &[Arc<dyn EngineSource<R>>],
        start: usize,
        count: usize,
        round_timeout: Duration,
    ) -> Result<ExtendResult<R>, CacheError> {
        let lock_key = format!("{}:{}", self.namespace, query);
        let lock = lock_for_query(lock_key.clone()).await;
        let _guard = lock.lock().await;

        let mut tx = self.pool.begin().await?;
        let namespace_id = db::get_or_create_namespace(&mut tx, self.namespace).await?;
        let query_id =
            db::get_or_create_query(&mut tx, query, namespace_id, chrono::Utc::now().naive_utc())
                .await?;
        tx.commit().await?;

        let mut merged = db::get_merged_rows::<R>(&self.pool, query_id).await?;
        let initial_len = merged.len();
        let needed_end = start + count;

        let mut next_start: HashMap<&'static str, i64> = HashMap::new();
        let mut exhausted: HashMap<&'static str, bool> = HashMap::new();
        for src in sources {
            let (ns, ex) = db::get_progress(&self.pool, query_id, src.name()).await?;
            next_start.insert(src.name(), ns);
            exhausted.insert(src.name(), ex);
        }

        let mut engine_outcomes: Vec<(String, EngineOutcome)> = Vec::new();

        for _round in 0..MAX_ROUNDS {
            if merged.len() >= needed_end {
                break;
            }

            let needy: Vec<Arc<dyn EngineSource<R>>> = sources
                .iter()
                .filter(|s| !exhausted[s.name()])
                .cloned()
                .collect();
            if needy.is_empty() {
                break;
            }

            let mut set = JoinSet::new();
            for src in needy {
                let q = query.to_string();
                let start_for_src = next_start[src.name()] as usize;
                set.spawn(async move {
                    let outcome = timeout(round_timeout, src.fetch_page(&q, start_for_src)).await;
                    (src.name(), outcome)
                });
            }
            let round_results = set.join_all().await;

            let existing_urls: HashMap<String, i64> =
                merged.iter().map(|r| (r.url.clone(), r.row_id)).collect();
            let mut batch_index: HashMap<String, usize> = HashMap::new();
            let mut fresh_batch: Vec<(R, Vec<String>)> = Vec::new();
            let mut attribute_existing: Vec<(i64, String)> = Vec::new();
            let mut any_new = false;

            for (name, outcome) in round_results {
                match outcome {
                    Ok(Ok(rows)) => {
                        let raw_count = rows.len();
                        for row in rows {
                            let url = row.url().to_string();
                            if let Some(&row_id) = existing_urls.get(&url) {
                                attribute_existing.push((row_id, name.to_string()));
                            } else if let Some(&idx) = batch_index.get(&url) {
                                fresh_batch[idx].1.push(name.to_string());
                            } else {
                                batch_index.insert(url, fresh_batch.len());
                                fresh_batch.push((row, vec![name.to_string()]));
                                any_new = true;
                            }
                        }
                        exhausted.insert(name, raw_count == 0);
                        next_start.insert(name, next_start[name] + raw_count as i64);
                        engine_outcomes.push((name.to_string(), EngineOutcome::Ok));
                    }
                    Ok(Err(e)) => {
                        log::warn!("source \"{name}\" failed: {e}");
                        engine_outcomes.push((name.to_string(), EngineOutcome::Failed(e)));
                    }
                    Err(_) => {
                        log::warn!("source \"{name}\" timed out");
                        engine_outcomes.push((name.to_string(), EngineOutcome::TimedOut));
                    }
                }
            }

            let mut tx = self.pool.begin().await?;
            db::touch_query(&mut tx, query_id, chrono::Utc::now().naive_utc()).await?;
            for src in sources {
                db::set_progress(
                    &mut tx,
                    query_id,
                    src.name(),
                    next_start[src.name()],
                    exhausted[src.name()],
                )
                .await?;
            }
            for (row_id, engine_name) in &attribute_existing {
                db::attribute_engine(&mut tx, query_id, *row_id, engine_name).await?;
            }

            let mut engines_by_url: HashMap<String, Vec<String>> = fresh_batch
                .iter()
                .map(|(r, e)| (r.url().to_string(), e.clone()))
                .collect();
            let ranked_rows = self
                .ranker
                .rank(query, fresh_batch.into_iter().map(|(r, _)| r).collect());

            let mut next_index = merged.len() as i64;
            for row in ranked_rows {
                let url = row.url().to_string();
                let engines = engines_by_url.remove(&url).unwrap_or_default();
                let row_id = db::get_or_create_row(&mut tx, &url, &row).await?;
                db::link_query_row(&mut tx, query_id, row_id, next_index).await?;
                for engine_name in &engines {
                    db::attribute_engine(&mut tx, query_id, row_id, engine_name).await?;
                }
                merged.push(db::MergedRow {
                    row_id,
                    url,
                    value: row,
                    engines,
                });
                next_index += 1;
            }
            tx.commit().await?;

            for (row_id, engine_name) in attribute_existing {
                if let Some(r) = merged.iter_mut().find(|r| r.row_id == row_id)
                    && !r.engines.contains(&engine_name)
                {
                    r.engines.push(engine_name);
                }
            }

            if !any_new {
                break;
            }
        }

        drop(_guard);
        release_query_lock(&lock_key, lock);

        let end = merged.len().min(needed_end);
        let start = start.min(end);
        let has_more = end < merged.len() || sources.iter().any(|s| !exhausted[s.name()]);

        let rows = merged[start..end]
            .iter()
            .enumerate()
            .map(|(i, r)| MergedRowResult {
                value: r.value.clone(),
                engines: r.engines.clone(),
                cached: start + i < initial_len,
            })
            .collect();

        Ok(ExtendResult {
            rows,
            has_more,
            engine_outcomes,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{
        collections::VecDeque,
        str::FromStr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
    struct TestRow {
        url: String,
        title: String,
    }

    impl CacheableRow for TestRow {
        fn url(&self) -> &str {
            &self.url
        }
    }

    fn row(url: &str) -> TestRow {
        TestRow {
            url: url.to_string(),
            title: format!("title for {url}"),
        }
    }

    struct NoopRanker;
    impl Ranker<TestRow> for NoopRanker {
        fn rank(&self, _query: &str, batch: Vec<TestRow>) -> Vec<TestRow> {
            batch
        }
    }

    /// A source whose pages are scripted in advance; each call pops the next
    /// page (or `[]` once drained) regardless of `start`, and counts calls so
    /// tests can assert whether the network/upstream was actually hit.
    struct ScriptedSource {
        name: &'static str,
        pages: StdMutex<VecDeque<Vec<TestRow>>>,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptedSource {
        fn new(name: &'static str, pages: Vec<Vec<TestRow>>) -> Arc<Self> {
            Arc::new(Self {
                name,
                pages: StdMutex::new(pages.into()),
                calls: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait]
    impl EngineSource<TestRow> for ScriptedSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn fetch_page(&self, _query: &str, _start: usize) -> Result<Vec<TestRow>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.pages.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    /// An isolated in-memory pool per test — deliberately avoids `db::init`'s
    /// env-var-based file path, since tests run concurrently in one process
    /// and a shared global env var races across threads (each test would
    /// risk connecting to whichever path a *different*, concurrently-running
    /// test just set). `max_connections(1)` keeps every checkout on the same
    /// `:memory:` database instead of each connection getting its own.
    async fn test_cache() -> MergedCache<TestRow> {
        let options = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        db::create_schema(&pool).await.unwrap();
        MergedCache::new(pool, "test", Arc::new(NoopRanker))
    }

    fn sources(v: Vec<Arc<ScriptedSource>>) -> Vec<Arc<dyn EngineSource<TestRow>>> {
        v.into_iter()
            .map(|s| s as Arc<dyn EngineSource<TestRow>>)
            .collect()
    }

    fn one_source(s: Arc<ScriptedSource>) -> Vec<Arc<dyn EngineSource<TestRow>>> {
        sources(vec![s])
    }

    #[tokio::test]
    async fn fresh_query_reports_has_more_until_a_source_proves_exhaustion() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b"), row("c")]]);

        let result = cache
            .get_or_extend("q", &one_source(source), 0, 3, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 3);
        assert!(
            result.has_more,
            "a full page doesn't prove the source is exhausted yet"
        );
        assert!(result.rows.iter().all(|r| !r.cached));
    }

    /// The direct regression test for "can't paginate a query that's already
    /// fully cached": once a merged window is already satisfied by
    /// previously-persisted rows, extending pagination into it must not
    /// touch the source at all.
    #[tokio::test]
    async fn cache_hit_pagination_never_recontacts_a_satisfied_source() {
        let cache = test_cache().await;
        let source = ScriptedSource::new(
            "A",
            vec![(0..12).map(|i| row(&format!("u{i}"))).collect()],
        );
        let calls = source.calls.clone();

        let page1 = cache
            .get_or_extend("q", &one_source(source.clone()), 0, 5, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(page1.rows.len(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Simulate a totally separate later request (e.g. a different
        // client/process) for page 2 — everything it needs is already in the
        // merged cache from page 1's over-fetch.
        let page2 = cache
            .get_or_extend("q", &one_source(source.clone()), 5, 5, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(page2.rows.len(), 5);
        assert!(page2.rows.iter().all(|r| r.cached));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "page 2 was fully covered by the existing merged cache — the source must not be re-hit"
        );
        // No gaps/dupes between page1 and page2.
        let urls1: Vec<_> = page1.rows.iter().map(|r| r.value.url.clone()).collect();
        let urls2: Vec<_> = page2.rows.iter().map(|r| r.value.url.clone()).collect();
        assert!(urls1.iter().all(|u| !urls2.contains(u)));
        assert_eq!(urls2[0], "u5");
    }

    #[tokio::test]
    async fn has_more_reflects_the_source_that_is_still_alive() {
        let cache = test_cache().await;
        let short = ScriptedSource::new("Short", vec![vec![row("s1")], vec![]]);
        let long = ScriptedSource::new(
            "Long",
            vec![vec![row("l1"), row("l2")], vec![row("l3"), row("l4")], vec![]],
        );

        let result = cache
            .get_or_extend(
                "q",
                &sources(vec![short, long]),
                0,
                5,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert!(result.has_more, "Long hasn't proven exhaustion within the rounds needed to fill the window");
    }

    #[tokio::test]
    async fn engine_attribution_accumulates_across_rounds_and_sources() {
        let cache = test_cache().await;
        // Round 1: both sources happen to surface the same URL in the same round.
        let a = ScriptedSource::new("A", vec![vec![row("shared")], vec![]]);
        let b = ScriptedSource::new("B", vec![vec![row("shared")], vec![]]);

        let result = cache
            .get_or_extend("q", &sources(vec![a, b]), 0, 1, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        let mut engines = result.rows[0].engines.clone();
        engines.sort();
        assert_eq!(engines, vec!["A".to_string(), "B".to_string()]);

        // A brand new source rediscovering the same URL in a later call
        // attributes to the existing row instead of duplicating it.
        let c = ScriptedSource::new("C", vec![vec![row("shared")]]);
        let result2 = cache
            .get_or_extend("q", &one_source(c), 1, 1, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(result2.rows.len(), 0, "no new merged row — it's the same URL");

        let result3 = cache
            .get_or_extend(
                "q",
                &one_source(ScriptedSource::new("D", vec![])),
                0,
                1,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let mut engines3 = result3.rows[0].engines.clone();
        engines3.sort();
        assert_eq!(engines3, vec!["A".to_string(), "B".to_string(), "C".to_string()]);
    }

    #[tokio::test]
    async fn purge_stale_removes_old_queries_and_orphaned_rows() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("old")]]);
        cache
            .get_or_extend("stale query", &one_source(source), 0, 1, Duration::from_secs(1))
            .await
            .unwrap();

        // Backdate it directly, then purge with a cutoff that only catches it.
        let (query_id,): (i64,) = sqlx::query_as("SELECT id FROM queries WHERE query = 'stale query'")
            .fetch_one(&cache.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE queries SET fetched_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().naive_utc() - chrono::Duration::days(30))
            .bind(query_id)
            .execute(&cache.pool)
            .await
            .unwrap();

        let cutoff = chrono::Utc::now().naive_utc() - chrono::Duration::days(7);
        let purged = db::purge_stale_queries(&cache.pool, cutoff).await.unwrap();
        assert_eq!(purged, 1);

        let (row_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rows")
            .fetch_one(&cache.pool)
            .await
            .unwrap();
        assert_eq!(row_count, 0, "orphaned rows should be swept");
    }

    /// A source that counts invocations and sleeps briefly, so overlapping
    /// callers genuinely race rather than trivially serializing.
    struct SlowCountingSource {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl EngineSource<TestRow> for SlowCountingSource {
        fn name(&self) -> &'static str {
            "Slow"
        }

        async fn fetch_page(&self, _query: &str, start: usize) -> Result<Vec<TestRow>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            if start > 0 {
                return Ok(Vec::new());
            }
            Ok((0..5).map(|i| row(&format!("s{i}"))).collect())
        }
    }

    /// Two overlapping requests for the identical query shouldn't both miss
    /// the cache and fire off redundant source calls (and concurrent,
    /// potentially lock-contending, DB writes) — the second should block on
    /// the per-query lock and then reuse what the first cached.
    #[tokio::test]
    async fn concurrent_identical_queries_only_hit_the_source_once() {
        let cache = test_cache().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let source: Arc<dyn EngineSource<TestRow>> = Arc::new(SlowCountingSource {
            calls: calls.clone(),
        });
        let src_vec = vec![source];

        let (a, b) = tokio::join!(
            cache.get_or_extend("dedup race", &src_vec, 0, 5, Duration::from_secs(1)),
            cache.get_or_extend("dedup race", &src_vec, 0, 5, Duration::from_secs(1)),
        );

        assert_eq!(a.unwrap().rows.len(), 5);
        assert_eq!(b.unwrap().rows.len(), 5);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "two concurrent identical queries should only hit the source once"
        );
    }

    #[tokio::test]
    async fn arbitrary_row_payload_round_trips_through_json_storage() {
        let cache = test_cache().await;
        let weird = row("https://example.com/日本語?q=café ☕ <script>&x=\"quoted\"");
        let source = ScriptedSource::new("A", vec![vec![weird.clone()]]);

        let result = cache
            .get_or_extend("unicode", &one_source(source), 0, 1, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(result.rows[0].value, weird);
    }
}
