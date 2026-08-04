//! Fan out a query across privacy-respecting search engines, cache/rank/paginate
//! the results, and expose a simple search API.
//!
//! This crate is the *opinionated* glue layer: it wires the pure engine
//! adapters in [`search_engines`] to the generic ranked-merge cache in
//! [`search_cache`], supplying our own domain-word ranking policy. A
//! different consumer who wants different ranking (or a different cache
//! backend) can depend on `search-engines`/`search-cache` directly instead
//! of this crate.
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

use async_trait::async_trait;
use search_cache::{CacheableRow, EngineOutcome, EngineSource, MergedCache, Ranker};
use search_engines::{Brave, DuckDuckGo, EngineInfo, ImageEngine, SearchEngine};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, fmt, sync::Arc, time::Duration};
use tokio::sync::OnceCell;

const ENGINE_TIMEOUT: u64 = 3; // seconds
const DEFAULT_SEARCH_COUNT: usize = 10;
const DEFAULT_IMAGE_COUNT: usize = 50;
/// Hint passed to an engine adapter's own page size — most of ours ignore it
/// and return whatever a real page contains (see `search-engines`).
const ENGINE_PAGE_HINT: usize = 20;

pub async fn init_db() {
    search_cache::shared_pool().await;
}

/// Purges cached queries (and their now-orphaned rows) older than `max_age`.
/// Returns the number of queries purged.
///
/// This crate never calls this on its own — callers (e.g. the `private-search`
/// binary) are expected to schedule it periodically, since only they know
/// what cadence/retention makes sense for their deployment.
pub async fn clean_cache(max_age: Duration) -> Result<u64, FetchError> {
    let pool = search_cache::shared_pool().await;
    Ok(search_cache::clean_cache(pool, max_age).await?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedResult {
    url: String,
    title: String,
    description: String,
}

impl CacheableRow for CachedResult {
    fn url(&self) -> &str {
        &self.url
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedImage {
    url: String,
    title: String,
}

impl CacheableRow for CachedImage {
    fn url(&self) -> &str {
        &self.url
    }
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
    Cache(search_cache::CacheError),
    /// Every requested engine was contacted this call and every one of them
    /// failed/timed out, and there was nothing already cached to fall back
    /// on — a genuine total outage, not just "no more results".
    AllEnginesFailed,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Cache(e) => write!(f, "cache error: {e}"),
            FetchError::AllEnginesFailed => write!(f, "all engines failed"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<search_cache::CacheError> for FetchError {
    fn from(e: search_cache::CacheError) -> Self {
        FetchError::Cache(e)
    }
}

/// How an individual engine fared on a given call; carried alongside the
/// merged results so callers (and UIs) can show e.g. "DuckDuckGo timed out".
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    TimedOut,
    Failed(String),
}

impl From<&EngineOutcome> for EngineStatus {
    fn from(o: &EngineOutcome) -> Self {
        match o {
            EngineOutcome::Ok => EngineStatus::Ok,
            EngineOutcome::TimedOut => EngineStatus::TimedOut,
            EngineOutcome::Failed(msg) => EngineStatus::Failed(msg.clone()),
        }
    }
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
    /// Whether there are more results beyond this page — a real exhaustion
    /// signal from the cache, not a "was this page full" guess.
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// Ranks a freshly-fetched batch by how many (non-stopword) query terms
/// appear in the result's domain — a cheap, deterministic relevance signal
/// good enough to prefer e.g. `rust-lang.org` over an unrelated blog post
/// for the query "rust". Runs once, at cache-insertion time, on each new
/// batch — never re-run over already-persisted rows (see [`search_cache`]'s
/// append-only merge order).
pub fn sort_results<T: CacheableRow>(mut results: Vec<T>, query: &str) -> Vec<T> {
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

    results.sort_by_cached_key(|r| std::cmp::Reverse(score(r.url(), &words)));
    results
}

struct DomainWordRanker;

impl Ranker<CachedResult> for DomainWordRanker {
    fn rank(&self, query: &str, batch: Vec<CachedResult>) -> Vec<CachedResult> {
        sort_results(batch, query)
    }
}

/// Images have no per-engine text relevance signal worth scoring — just a
/// deterministic order (alphabetical by URL) so pagination is stable.
struct UrlSortRanker;

impl Ranker<CachedImage> for UrlSortRanker {
    fn rank(&self, _query: &str, mut batch: Vec<CachedImage>) -> Vec<CachedImage> {
        batch.sort_by(|a, b| a.url.cmp(&b.url));
        batch
    }
}

struct BraveTextSource;

#[async_trait]
impl EngineSource<CachedResult> for BraveTextSource {
    fn name(&self) -> &'static str {
        Brave.name()
    }

    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<CachedResult>, String> {
        Brave
            .search_results(query, start, ENGINE_PAGE_HINT)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|r| CachedResult {
                        url: r.url,
                        title: r.title,
                        description: r.description,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
}

struct DdgTextSource;

#[async_trait]
impl EngineSource<CachedResult> for DdgTextSource {
    fn name(&self) -> &'static str {
        DuckDuckGo.name()
    }

    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<CachedResult>, String> {
        DuckDuckGo
            .search_results(query, start, ENGINE_PAGE_HINT)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|r| CachedResult {
                        url: r.url,
                        title: r.title,
                        description: r.description,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
}

struct BraveImageSource;

#[async_trait]
impl EngineSource<CachedImage> for BraveImageSource {
    fn name(&self) -> &'static str {
        Brave.name()
    }

    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<CachedImage>, String> {
        Brave
            .search_images(query, start, ENGINE_PAGE_HINT)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|r| CachedImage {
                        url: r.url,
                        title: r.title,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
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

    fn source(self) -> Arc<dyn EngineSource<CachedResult>> {
        match self {
            Self::Brave => Arc::new(BraveTextSource),
            Self::DuckDuckGo => Arc::new(DdgTextSource),
        }
    }
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

    fn source(self) -> Arc<dyn EngineSource<CachedImage>> {
        match self {
            Self::Brave => Arc::new(BraveImageSource),
        }
    }
}

static TEXT_CACHE: OnceCell<MergedCache<CachedResult>> = OnceCell::const_new();

async fn text_cache() -> &'static MergedCache<CachedResult> {
    TEXT_CACHE
        .get_or_init(|| async {
            let pool = search_cache::shared_pool().await.clone();
            MergedCache::new(pool, "text", Arc::new(DomainWordRanker))
        })
        .await
}

static IMAGE_CACHE: OnceCell<MergedCache<CachedImage>> = OnceCell::const_new();

async fn image_cache() -> &'static MergedCache<CachedImage> {
    IMAGE_CACHE
        .get_or_init(|| async {
            let pool = search_cache::shared_pool().await.clone();
            MergedCache::new(pool, "image", Arc::new(UrlSortRanker))
        })
        .await
}

/// True only when there is nothing usable to return: every requested engine
/// was actually contacted this call, and none of them succeeded.
fn all_contacted_engines_failed(outcomes: &[(String, EngineOutcome)]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|(_, o)| !matches!(o, EngineOutcome::Ok))
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

    /// Offset into the merged result list (for pagination). Default 0.
    pub fn start(mut self, start: usize) -> Self {
        self.start = start;
        self
    }

    /// Number of results to return. Default 10.
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Per-engine, per-round timeout. Default 3 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the search, extending the merged cache as needed and ranking any
    /// newly-discovered results by [`sort_results`].
    pub async fn search(self) -> Result<SearchResponse<SearchResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            SearchEngines::all()
        } else {
            self.engines
        };

        let sources: Vec<Arc<dyn EngineSource<CachedResult>>> =
            engines.iter().map(|e| e.source()).collect();

        let extend = text_cache()
            .await
            .get_or_extend(&self.query, &sources, self.start, self.count, self.timeout)
            .await?;

        if extend.rows.is_empty() && all_contacted_engines_failed(&extend.engine_outcomes) {
            return Err(FetchError::AllEnginesFailed);
        }

        let reports = engines
            .iter()
            .map(|e| {
                let name = e.name();
                let status = extend
                    .engine_outcomes
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, o)| EngineStatus::from(o))
                    .unwrap_or(EngineStatus::Ok);
                EngineReport {
                    engine: name.to_string(),
                    status,
                }
            })
            .collect();

        let results = extend
            .rows
            .into_iter()
            .map(|r| SearchResult {
                url: r.value.url,
                title: r.value.title,
                description: r.value.description,
                engines: r.engines,
                cached: r.cached,
            })
            .collect();

        Ok(SearchResponse {
            results,
            engines: reports,
            has_more: extend.has_more,
        })
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

    /// Offset into the merged result list (for pagination). Default 0.
    pub fn start(mut self, start: usize) -> Self {
        self.start = start;
        self
    }

    /// Number of images to return. Default 50.
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Per-engine, per-round timeout. Default 3 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs the search, extending the merged cache as needed.
    pub async fn search(self) -> Result<SearchResponse<ImageResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            ImageEngines::all()
        } else {
            self.engines
        };

        let sources: Vec<Arc<dyn EngineSource<CachedImage>>> =
            engines.iter().map(|e| e.source()).collect();

        let extend = image_cache()
            .await
            .get_or_extend(&self.query, &sources, self.start, self.count, self.timeout)
            .await?;

        if extend.rows.is_empty() && all_contacted_engines_failed(&extend.engine_outcomes) {
            return Err(FetchError::AllEnginesFailed);
        }

        let reports = engines
            .iter()
            .map(|e| {
                let name = e.name();
                let status = extend
                    .engine_outcomes
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, o)| EngineStatus::from(o))
                    .unwrap_or(EngineStatus::Ok);
                EngineReport {
                    engine: name.to_string(),
                    status,
                }
            })
            .collect();

        let results = extend
            .rows
            .into_iter()
            .map(|r| ImageResult {
                url: r.value.url,
                title: r.value.title,
                engines: r.engines,
                cached: r.cached,
            })
            .collect();

        Ok(SearchResponse {
            results,
            engines: reports,
            has_more: extend.has_more,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn r(url: &str) -> CachedResult {
        CachedResult {
            url: url.to_string(),
            title: "t".into(),
            description: "d".into(),
        }
    }

    #[test]
    fn sort_results_ranks_full_domain_match_first() {
        let ranked = sort_results(
            vec![
                r("https://unrelated.example/x"),
                r("https://partial-rust.example/x"),
                r("https://rust-async.example/x"),
            ],
            "rust async",
        );

        assert_eq!(ranked[0].url, "https://rust-async.example/x");
        assert_eq!(ranked[2].url, "https://unrelated.example/x");
    }

    #[test]
    fn sort_results_ranks_partial_domain_match_above_no_match() {
        let ranked = sort_results(
            vec![
                r("https://unrelated.example/x"),
                r("https://rust-only.example/x"),
            ],
            "rust async",
        );

        assert_eq!(ranked[0].url, "https://rust-only.example/x");
        assert_eq!(ranked[1].url, "https://unrelated.example/x");
    }

    #[test]
    fn sort_results_is_a_stable_sort_preserving_input_order_for_ties() {
        let ranked = sort_results(
            vec![r("https://a.example/x"), r("https://b.example/x")],
            "irrelevant query",
        );

        // No domain matches either way — score ties at 0, order unchanged.
        assert_eq!(ranked[0].url, "https://a.example/x");
        assert_eq!(ranked[1].url, "https://b.example/x");
    }

    #[test]
    fn sort_results_ignores_stopwords_when_scoring() {
        // "for" is a stopword; only "rust" should count toward the score.
        let ranked = sort_results(
            vec![
                r("https://unrelated.example/x"),
                r("https://rust.example/x"),
            ],
            "for rust",
        );

        assert_eq!(ranked[0].url, "https://rust.example/x");
    }

    /// Regression guard for "encoding/JSON support": unicode titles/
    /// descriptions must survive a `serde_json` round trip unchanged.
    #[test]
    fn search_result_serializes_unicode_fields_unchanged() {
        let result = SearchResult {
            url: "https://example.com/日本語".to_string(),
            title: "café ☕ — \"quoted\" <tag>".to_string(),
            description: "مرحبا بالعالم (RTL text) 🎉".to_string(),
            engines: vec!["Brave".to_string()],
            cached: false,
        };

        let json = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["url"], "https://example.com/日本語");
        assert_eq!(value["title"], "café ☕ — \"quoted\" <tag>");
        assert_eq!(value["description"], "مرحبا بالعالم (RTL text) 🎉");
    }

    #[ignore]
    #[tokio::test]
    async fn test_search_builder_pagination_live() {
        // Safe: run in isolation via `--ignored --test-threads=1`, so nothing
        // else touches the process-global cache pool concurrently.
        unsafe {
            std::env::set_var(
                "CACHE_DB_PATH",
                "/tmp/private-search-engines-pagination-test.db",
            );
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
