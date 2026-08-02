#![allow(async_fn_in_trait)]
//! Fan out a query across privacy-respecting search engines, cache results in SQLite, merge and rank them.
//!
//! [`SearchBuilder`] and [`ImageSearchBuilder`] are the main entry points:
//!
//! ```no_run
//! # async fn run() -> Result<(), private_search_engines::FetchError> {
//! use private_search_engines::{SearchBuilder, SearchEngines};
//!
//! let results = SearchBuilder::new("rust async").search().await?;
//!
//! let results = SearchBuilder::new("rust async")
//!     .engine(SearchEngines::Brave)
//!     .count(20)
//!     .search()
//!     .await?;
//! # Ok(())
//! # }
//! ```

use serde::Serialize;
use sqlx::SqlitePool;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    fmt,
    pin::Pin,
    time::Duration,
};
use tokio::{sync::OnceCell, task::JoinSet, time::timeout};

use crate::{
    cache::{ImagesRow, ResultRow},
    engines::{Brave, DuckDuckGo, EngineError, EngineInfo, ImageEngine, SearchEngine},
};

mod cache;
pub mod engines;

const ENGINE_TIMEOUT: u64 = 3; // seconds
const DEFAULT_SEARCH_COUNT: usize = 10;
const DEFAULT_IMAGE_COUNT: usize = 50;
/// Caps extra engine-page fetches per call, so a deep `start` or broken pagination can't loop forever.
const MAX_ENGINE_PAGES_PER_CALL: usize = 10;

static SQLPOOL: OnceCell<SqlitePool> = OnceCell::const_new();

async fn get_db() -> &'static SqlitePool {
    SQLPOOL
        .get_or_init(|| async { cache::init().await.expect("Failed to init cache db") })
        .await
}

pub async fn init_db() {
    get_db().await;
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub description: String,
    pub engines: Vec<String>,
    pub cached: bool,
}

impl PartialEq for SearchResult {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl PartialOrd for SearchResult {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.url.cmp(&other.url))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageResult {
    pub url: String,
    pub title: String,
    pub engines: Vec<String>,
    pub cached: bool,
}

impl PartialEq for ImageResult {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl PartialOrd for ImageResult {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.url.cmp(&other.url))
    }
}

#[derive(Debug)]
pub enum FetchError {
    Sqlx(sqlx::Error),
    Engine(EngineError),
    AllEnginesFailed,
    Timeouts,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Sqlx(e) => write!(f, "cache error: {e}"),
            FetchError::Engine(e) => write!(f, "engine error: {e:?}"),
            FetchError::AllEnginesFailed => write!(f, "all engines failed"),
            FetchError::Timeouts => write!(f, "search timed out"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<sqlx::Error> for FetchError {
    fn from(e: sqlx::Error) -> Self {
        FetchError::Sqlx(e)
    }
}

impl From<EngineError> for FetchError {
    fn from(e: EngineError) -> Self {
        FetchError::Engine(e)
    }
}

type EngineFuture<T> = Pin<Box<dyn Future<Output = Result<Vec<T>, FetchError>> + Send>>;

/// How an individual engine fared on a given call; carried alongside the
/// merged results so callers (and UIs) can show e.g. "DuckDuckGo timed out".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    TimedOut,
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineReport {
    pub engine: String,
    pub status: EngineStatus,
}

/// Results from a [`SearchBuilder`] or [`ImageSearchBuilder`] call, paired
/// with a per-engine status report so callers can surface timeouts/failures
/// alongside the (possibly partial) results.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse<T> {
    pub results: Vec<T>,
    pub engines: Vec<EngineReport>,
}

/// Runs engine futures concurrently under a shared timeout; fails only if every engine failed.
async fn gather<T: Send + 'static>(
    futures: Vec<(&'static str, EngineFuture<T>)>,
    timeout_duration: Duration,
) -> Result<(Vec<T>, Vec<EngineReport>), FetchError> {
    let mut set = JoinSet::new();

    for (name, fut) in futures {
        set.spawn(async move { (name, timeout(timeout_duration, fut).await) });
    }

    let per_engine = timeout(timeout_duration, set.join_all())
        .await
        .map_err(|_| FetchError::Timeouts)?;

    let mut flat = Vec::new();
    let mut reports = Vec::new();
    let mut any_success = false;

    for (name, engine_result) in per_engine {
        match engine_result {
            Ok(Ok(rows)) => {
                any_success = true;
                flat.extend(rows);
                reports.push(EngineReport {
                    engine: name.to_string(),
                    status: EngineStatus::Ok,
                });
            }
            Ok(Err(e)) => {
                eprintln!("Engine failed: {e}");
                reports.push(EngineReport {
                    engine: name.to_string(),
                    status: EngineStatus::Failed(e.to_string()),
                });
            }
            Err(_) => {
                eprintln!("Engine timed out");
                reports.push(EngineReport {
                    engine: name.to_string(),
                    status: EngineStatus::TimedOut,
                });
            }
        }
    }

    if !any_success {
        return Err(FetchError::AllEnginesFailed);
    }

    Ok((flat, reports))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngines {
    Brave,
    DuckDuckGo,
}

impl SearchEngines {
    /// Every known text-search engine; the default set for [`SearchBuilder`].
    pub fn all() -> Vec<Self> {
        vec![Self::Brave, Self::DuckDuckGo]
    }

    fn name(self) -> &'static str {
        match self {
            Self::Brave => Brave.name(),
            Self::DuckDuckGo => DuckDuckGo.name(),
        }
    }

    fn fetch(self, query: String, start: usize, count: usize) -> EngineFuture<SearchResult> {
        match self {
            Self::Brave => Box::pin(fetch_or_cache_result(Brave, query, start, count)),
            Self::DuckDuckGo => Box::pin(fetch_or_cache_result(DuckDuckGo, query, start, count)),
        }
    }
}

/// Builds and runs a text search across one or more engines.
///
/// Defaults: every engine in [`SearchEngines::all`], 10 results from 0, 3s timeout.
pub struct SearchBuilder {
    query: String,
    engines: Vec<SearchEngines>,
    start: usize,
    count: usize,
    timeout: Duration,
}

impl SearchBuilder {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            engines: Vec::new(),
            start: 0,
            count: DEFAULT_SEARCH_COUNT,
            timeout: Duration::from_secs(ENGINE_TIMEOUT),
        }
    }

    /// Adds an engine to query; duplicates are ignored. Defaults to all engines if never called.
    pub fn engine(mut self, engine: SearchEngines) -> Self {
        if !self.engines.contains(&engine) {
            self.engines.push(engine);
        }
        self
    }

    /// Adds several engines at once; same as calling [`engine`](Self::engine) per item.
    pub fn engines(mut self, engines: impl IntoIterator<Item = SearchEngines>) -> Self {
        for engine in engines {
            self = self.engine(engine);
        }
        self
    }

    /// Offset into each engine's result list (for pagination). Default 0.
    pub fn start(mut self, start: usize) -> Self {
        self.start = start;
        self
    }

    /// Number of results to fetch per engine. Default 10.
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Overall timeout for the whole search. Default 3 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the search, merging and ranking results from every configured engine.
    pub async fn search(self) -> Result<SearchResponse<SearchResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            SearchEngines::all()
        } else {
            self.engines
        };

        let futures = engines
            .into_iter()
            .map(|engine| (engine.name(), engine.fetch(self.query.clone(), self.start, self.count)))
            .collect();

        let (flat, engines) = gather(futures, self.timeout).await?;
        let merged = merge_results(flat);
        Ok(SearchResponse {
            results: sort_results(merged, &self.query),
            engines,
        })
    }
}

fn merge_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut map: BTreeMap<String, SearchResult> = BTreeMap::new();

    for row in results {
        let key = row.url.clone();

        map.entry(key)
            .and_modify(|existing| {
                existing.engines.extend(row.engines.clone());

                if existing.description.is_empty() {
                    existing.description = row.description.clone();
                }
                if existing.title.is_empty() {
                    existing.title = row.title.clone();
                }
            })
            .or_insert(row);
    }

    map.into_values().collect()
}

pub fn sort_results(mut results: Vec<SearchResult>, query: &str) -> Vec<SearchResult> {
    let stop = ["the", "and", "or", "of", "for", "in", "on", "at"];
    let words: Vec<String> = query
        .split_whitespace()
        .filter(|w| !stop.contains(&w.to_lowercase().as_str()))
        .map(str::to_lowercase)
        .collect();

    fn score(url: &str, words: &[String]) -> u8 {
        let dom = url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_lowercase();

        let hits = words.iter().filter(|w| dom.contains(*w)).count();
        if hits == words.len() {
            2
        }
        // all words
        else if hits > 0 {
            1
        }
        // some words
        else {
            0
        }
    }

    results.sort_by_cached_key(|r| std::cmp::Reverse(score(&r.url, &words)));
    results
}

/// Checks the cache first; if miss, fetches from the engine and caches results.
///
/// This is the building block [`SearchBuilder`] is implemented on top of; use
/// it directly if you're integrating an engine that isn't in [`SearchEngines`].
pub async fn fetch_or_cache_result<E>(
    engine: E,
    query: String,
    start: usize,
    count: usize,
) -> Result<Vec<SearchResult>, FetchError>
where
    E: SearchEngine + EngineInfo + Send,
{
    let pool = get_db().await;

    let engine_enum = engine.name();
    let engine_id = cache::get_engine_id(pool, engine_enum).await?;

    let mut cached_rows = if let Some(query_row) = cache::get_query(pool, &query, engine_id).await?
    {
        cache::get_results_for_query(pool, query_row.id).await?
    } else {
        Vec::new()
    };

    let initial_cached_len = cached_rows.len();
    let needed_end = start + count;
    let mut seen: HashSet<String> = cached_rows.iter().map(|r| r.url.clone()).collect();

    // Fetch further engine pages until the window is covered or the engine runs dry.
    for _ in 0..MAX_ENGINE_PAGES_PER_CALL {
        if cached_rows.len() >= needed_end {
            break;
        }

        let fetch_start = cached_rows.len();
        let engine_results = engine.search_results(&query, fetch_start, count).await?;

        let fresh: Vec<ResultRow> = engine_results
            .into_iter()
            .filter(|r| seen.insert(r.url.clone()))
            .collect();

        if fresh.is_empty() {
            break; // engine has no more (new) results
        }

        let fetched_at = chrono::Utc::now().naive_utc();
        cache::upsert_query_with_results(pool, engine_enum, &query, fresh.clone(), fetched_at)
            .await?;

        cached_rows.extend(fresh);
    }

    let end = cached_rows.len().min(needed_end);
    let start = start.min(end);

    Ok(cached_rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, cr)| SearchResult {
            url: cr.url.clone(),
            title: cr.title.clone(),
            description: cr.description.clone(),
            engines: vec![engine.name().to_string()],
            cached: start + i < initial_cached_len,
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEngines {
    Brave,
}

impl ImageEngines {
    /// Every known image-search engine; the default set for [`ImageSearchBuilder`].
    pub fn all() -> Vec<Self> {
        vec![Self::Brave]
    }

    fn name(self) -> &'static str {
        match self {
            Self::Brave => Brave.name(),
        }
    }

    fn fetch(self, query: String, start: usize, count: usize) -> EngineFuture<ImageResult> {
        match self {
            Self::Brave => Box::pin(fetch_or_cache_image(Brave, query, start, count)),
        }
    }
}

/// Builds and runs an image search across one or more engines.
///
/// Defaults: every engine in [`ImageEngines::all`], 50 results from 0, 3s timeout.
pub struct ImageSearchBuilder {
    query: String,
    engines: Vec<ImageEngines>,
    start: usize,
    count: usize,
    timeout: Duration,
}

impl ImageSearchBuilder {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            engines: Vec::new(),
            start: 0,
            count: DEFAULT_IMAGE_COUNT,
            timeout: Duration::from_secs(ENGINE_TIMEOUT),
        }
    }

    /// Adds an engine to query. May be called more than once; duplicates are
    /// ignored. If never called, all known engines are used.
    pub fn engine(mut self, engine: ImageEngines) -> Self {
        if !self.engines.contains(&engine) {
            self.engines.push(engine);
        }
        self
    }

    /// Adds several engines at once; same as calling [`engine`](Self::engine) per item.
    pub fn engines(mut self, engines: impl IntoIterator<Item = ImageEngines>) -> Self {
        for engine in engines {
            self = self.engine(engine);
        }
        self
    }

    /// Offset into each engine's result list (for pagination). Default 0.
    pub fn start(mut self, start: usize) -> Self {
        self.start = start;
        self
    }

    /// Number of images to fetch per engine. Default 50.
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Overall timeout for the whole search. Default 3 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the search, merging results from every configured engine.
    pub async fn search(self) -> Result<SearchResponse<ImageResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            ImageEngines::all()
        } else {
            self.engines
        };

        let futures = engines
            .into_iter()
            .map(|engine| (engine.name(), engine.fetch(self.query.clone(), self.start, self.count)))
            .collect();

        let (flat, engines) = gather(futures, self.timeout).await?;
        Ok(SearchResponse {
            results: merge_images(flat),
            engines,
        })
    }
}

fn merge_images(images: Vec<ImageResult>) -> Vec<ImageResult> {
    let mut map: BTreeMap<String, ImageResult> = BTreeMap::new();

    for row in images {
        let key = row.url.clone();

        map.entry(key)
            .and_modify(|existing| {
                existing.engines.extend(row.engines.clone());

                if existing.title.is_empty() {
                    existing.title = row.title.clone();
                }
            })
            .or_insert(row);
    }

    map.into_values().collect()
}

/// Checks the cache first; if miss, fetches from the engine and caches images.
///
/// This is the building block [`ImageSearchBuilder`] is implemented on top of;
/// use it directly if you're integrating an engine that isn't in [`ImageEngines`].
pub async fn fetch_or_cache_image<E>(
    engine: E,
    query: String,
    start: usize,
    count: usize,
) -> Result<Vec<ImageResult>, FetchError>
where
    E: ImageEngine + EngineInfo,
{
    let pool = get_db().await;

    let engine_enum = engine.name();
    let engine_id = cache::get_engine_id(pool, engine_enum).await?;

    let mut cached_rows = if let Some(query_row) = cache::get_query(pool, &query, engine_id).await?
    {
        cache::get_images_for_query(pool, query_row.id).await?
    } else {
        Vec::new()
    };

    let initial_cached_len = cached_rows.len();
    let needed_end = start + count;
    let mut seen: HashSet<String> = cached_rows.iter().map(|r| r.url.clone()).collect();

    // Same paging loop as fetch_or_cache_result; a no-op-pagination engine just
    // yields an empty `fresh` and exits after one page.
    for _ in 0..MAX_ENGINE_PAGES_PER_CALL {
        if cached_rows.len() >= needed_end {
            break;
        }

        let fetch_start = cached_rows.len();
        let engine_images = engine.search_images(&query, fetch_start, count).await?;

        let fresh: Vec<ImagesRow> = engine_images
            .into_iter()
            .filter(|r| seen.insert(r.url.clone()))
            .collect();

        if fresh.is_empty() {
            break;
        }

        let fetched_at = chrono::Utc::now().naive_utc();
        cache::upsert_query_with_images(pool, engine_enum, &query, fresh.clone(), fetched_at)
            .await?;

        cached_rows.extend(fresh);
    }

    let end = cached_rows.len().min(needed_end);
    let start = start.min(end);

    Ok(cached_rows[start..end]
        .iter()
        .enumerate()
        .map(|(i, cr)| ImageResult {
            url: cr.url.clone(),
            title: cr.title.clone(),
            engines: vec![engine.name().to_string()],
            cached: start + i < initial_cached_len,
        })
        .collect())
}

#[cfg(test)]
mod test {
    use super::*;

    /// End-to-end proof that pagination works through the full cache + builder
    /// pipeline: page 2 disjoint from page 1, page 1 revisited from cache.
    /// Hits real network + a scratch on-disk cache, so `#[ignore]`d.
    #[ignore]
    #[tokio::test]
    async fn test_search_builder_pagination_live() {
        // Safe: run in isolation via `--ignored --test-threads=1`, so nothing
        // else touches the process-global DB singleton concurrently.
        unsafe {
            std::env::set_var("CACHE_DB_PATH", "/tmp/private-search-engines-pagination-test.db");
        }
        let _ = std::fs::remove_file("/tmp/private-search-engines-pagination-test.db");

        let query = "rust async";

        let page1 = SearchBuilder::new(query)
            .engine(SearchEngines::Brave)
            .start(0)
            .count(20)
            .search()
            .await
            .unwrap()
            .results;
        let page2 = SearchBuilder::new(query)
            .engine(SearchEngines::Brave)
            .start(20)
            .count(20)
            .search()
            .await
            .unwrap()
            .results;

        assert!(!page1.is_empty());
        assert!(!page2.is_empty());
        assert!(
            page1.iter().all(|a| !page2.iter().any(|b| a.url == b.url)),
            "page 2 repeated page 1's results — pagination isn't advancing"
        );
        assert!(
            page1.iter().all(|r| !r.cached),
            "first-ever fetch shouldn't report anything as cached"
        );

        let page1_again = SearchBuilder::new(query)
            .engine(SearchEngines::Brave)
            .start(0)
            .count(20)
            .search()
            .await
            .unwrap()
            .results;
        assert!(
            page1_again.iter().all(|r| r.cached),
            "revisiting page 1 should be served entirely from cache"
        );

        let _ = std::fs::remove_file("/tmp/private-search-engines-pagination-test.db");
    }
}
