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
use search_cache::{
    CacheEvent, CacheSubscription, CacheableRow, EngineOutcome, EngineSource, MergedCache,
    MergedRowResult, OrderKind, Ranker, SequencedCacheEvent,
};
use search_engines::{
    ArchWiki, Brave, Codeberg, CratesIo, DockerHub, DuckDuckGo, EngineInfo, GitHub, GitLab, GoPkg,
    HackerNews, HexPm, ImageEngine, Lobsters, MavenCentral, Mdn, NixPackages, Npm, NuGet,
    Packagist, PyPi, RubyGems, SearchEngine, StackExchange, Wikipedia,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{OnceCell, mpsc};

const ENGINE_TIMEOUT: u64 = 8; // seconds
const DEFAULT_SEARCH_COUNT: usize = 10;
const DEFAULT_IMAGE_COUNT: usize = 50;
/// Hint passed to an engine adapter's own page size — most of ours ignore it
/// and return whatever a real page contains (see `search-engines`).
const ENGINE_PAGE_HINT: usize = 20;

/// Once an engine fails or times out, contacting it again on every
/// subsequent `/query` request is exactly the sustained futile traffic that
/// keeps an IP-reputation-based soft block (e.g. DuckDuckGo's bot wall) from
/// ever decaying into an unblock. This registry makes an engine that just
/// failed sit out for a growing window instead of being hit again
/// immediately: the window escalates on consecutive failures and resets the
/// instant the engine succeeds again.
struct CooldownState {
    consecutive_failures: u32,
    cooling_until: Option<Instant>,
}

static COOLDOWNS: OnceLock<StdMutex<HashMap<&'static str, CooldownState>>> = OnceLock::new();

fn cooldowns() -> &'static StdMutex<HashMap<&'static str, CooldownState>> {
    COOLDOWNS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Reads `env_var` as a `u64` seconds count, falling back to `default_secs`
/// if unset or unparseable. Mirrors the same-named helper in
/// `private-search/src/main.rs`; duplicated here rather than shared because
/// it's three lines and this crate has no other reason to depend on the
/// binary crate (which depends on it, not the other way around).
fn resolve_secs(env_var: &str, default_secs: u64) -> Duration {
    std::env::var(env_var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

/// Base of the escalating cooldown window (default 60s). Setting this to 0
/// via `ENGINE_COOLDOWN_BASE_SECS` disables cooldowns entirely, without a
/// rebuild — every call to `record_failure` becomes a no-op and
/// `cooldown_remaining` always reports the engine as usable.
fn default_engine_timeout() -> Duration {
    static TIMEOUT: OnceLock<Duration> = OnceLock::new();
    *TIMEOUT.get_or_init(|| resolve_secs("ENGINE_TIMEOUT_SECS", ENGINE_TIMEOUT))
}

fn cooldown_base() -> Duration {
    static BASE: OnceLock<Duration> = OnceLock::new();
    *BASE.get_or_init(|| resolve_secs("ENGINE_COOLDOWN_BASE_SECS", 60))
}

/// Cap on the escalating cooldown window (default 1800s / 30 minutes).
fn cooldown_cap() -> Duration {
    static CAP: OnceLock<Duration> = OnceLock::new();
    *CAP.get_or_init(|| resolve_secs("ENGINE_COOLDOWN_MAX_SECS", 1800))
}

/// `None` if `engine` is usable right now (never failed, or its cooldown has
/// already elapsed); otherwise the time remaining before it's usable again.
fn cooldown_remaining(engine: &'static str) -> Option<Duration> {
    let until = cooldowns().lock().unwrap().get(engine)?.cooling_until?;
    let now = Instant::now();
    (until > now).then(|| until - now)
}

/// Marks `engine` as having just failed/timed out, escalating its cooldown:
/// the 1st consecutive failure sets `cooldown_base()`, then it doubles per
/// additional consecutive failure, capped at `cooldown_cap()`. A no-op if
/// cooldowns are disabled (`cooldown_base()` is zero).
fn record_failure(engine: &'static str) {
    let base = cooldown_base();
    if base.is_zero() {
        return;
    }
    let mut map = cooldowns().lock().unwrap();
    let state = map.entry(engine).or_insert(CooldownState {
        consecutive_failures: 0,
        cooling_until: None,
    });
    state.consecutive_failures += 1;
    // Shift amount is capped well below 64 so this can't overflow even if a
    // test (or a very long-running process) drives the failure count high;
    // `saturating_mul` below then caps the resulting duration long before it
    // could overflow `Instant` addition, since `.min(cooldown_cap())` bounds
    // it to a realistic, operator-configured ceiling.
    let exponent = (state.consecutive_failures - 1).min(32);
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let backoff =
        Duration::from_secs(base.as_secs().saturating_mul(multiplier)).min(cooldown_cap());
    state.cooling_until = Some(Instant::now() + backoff);
}

/// Clears any cooldown/failure-count for `engine` — called on an `Ok`
/// outcome so a subsequently-healthy engine isn't left cooling down from an
/// earlier, now-irrelevant, streak of failures.
fn record_success(engine: &'static str) {
    cooldowns().lock().unwrap().remove(engine);
}

async fn wait_for_engine(engine: &'static str) -> Result<(), String> {
    static BRAVE: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    static DUCKDUCKGO: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let gate = match engine {
        "Brave" => &BRAVE,
        "DuckDuckGo" => &DUCKDUCKGO,
        _ => return Ok(()),
    };
    reserve_engine_slot(
        gate.get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now())),
        engine,
        resolve_secs("ENGINE_REQUEST_INTERVAL_SECS", 3),
    )
    .await
}

async fn reserve_engine_slot(
    gate: &tokio::sync::Mutex<tokio::time::Instant>,
    engine: &'static str,
    interval: Duration,
) -> Result<(), String> {
    let mut next_request = gate.lock().await;
    if let Some(remaining) = cooldown_remaining(engine) {
        return Err(format!("retry in {}s", remaining.as_secs()));
    }
    tokio::time::sleep_until(*next_request).await;
    if let Some(remaining) = cooldown_remaining(engine) {
        return Err(format!("retry in {}s", remaining.as_secs()));
    }
    *next_request = tokio::time::Instant::now()
        .checked_add(interval)
        .ok_or_else(|| "ENGINE_REQUEST_INTERVAL_SECS is too large".to_owned())?;
    Ok(())
}

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

pub use search_engines::proxy::{ProxyStats, stats as proxy_stats};

pub fn proxy_health_enabled() -> bool {
    search_engines::proxy::enabled()
        || matches!(
            std::env::var("ENGINE_PROXY_HEALTH_ENABLED").as_deref(),
            Ok("1")
        )
}

pub async fn refresh_proxies() -> Result<ProxyStats, String> {
    search_engines::proxy::refresh_once().await
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
    pub score: u32,
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
    /// Every requested engine was skipped this call because each one is
    /// still cooling down from a recent failure — so, unlike
    /// `AllEnginesFailed`, *nothing was contacted this call, by design* —
    /// and there was nothing already cached to fall back on either. This is
    /// expected, self-healing behavior (the cooldown doing its job), not a
    /// fault.
    AllEnginesCoolingDown,
}

impl FetchError {
    pub fn is_unknown_order(&self) -> bool {
        matches!(self, Self::Cache(search_cache::CacheError::UnknownOrder))
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Cache(e) => write!(f, "cache error: {e}"),
            FetchError::AllEnginesFailed => write!(f, "all engines failed"),
            FetchError::AllEnginesCoolingDown => write!(f, "all engines cooling down"),
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
    /// Skipped because of an active cooldown or an unavailable pagination session.
    CoolingDown(String),
}

impl From<&EngineOutcome> for EngineStatus {
    fn from(o: &EngineOutcome) -> Self {
        match o {
            EngineOutcome::Ok => EngineStatus::Ok,
            EngineOutcome::TimedOut => EngineStatus::TimedOut,
            EngineOutcome::Failed(msg) => EngineStatus::Failed(msg.clone()),
            EngineOutcome::Deferred(msg) => EngineStatus::CoolingDown(msg.clone()),
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

// --- Streaming event types --------------------------------------------------
//
// The browser-facing payload contract, one struct/enum per named SSE event:
// `meta` -> `StreamMeta`, `results` -> `StreamResults<T>`, `attribution` ->
// `StreamAttribution`, `engine` -> `EngineReport` (reused as-is), `done` ->
// `StreamDone`, `error` -> `StreamErrorPayload`. `StreamEvent<T>` wraps all
// six, serialized untagged so the wire payload is exactly the inner struct
// (the HTTP layer supplies the SSE `event:` name separately via `.name()`).

/// Which order a stream is building/reading from, and whether it was (at
/// least initially) served from cache without contacting any engine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamMeta {
    pub order_id: i64,
    pub canonical: bool,
    pub cached: bool,
}

/// One result at its final position in the active order.
#[derive(Debug, Clone, Serialize)]
pub struct PositionedResult<T> {
    pub position: usize,
    pub result: T,
}

/// A batch of newly-committed-or-replayed results.
#[derive(Debug, Clone, Serialize)]
pub struct StreamResults<T> {
    pub entries: Vec<PositionedResult<T>>,
}

/// `position`'s row gained (or already had) another contributing engine —
/// carries its full, current engine list as of this event.
#[derive(Debug, Clone, Serialize)]
pub struct StreamAttribution {
    pub position: usize,
    pub url: String,
    pub engines: Vec<String>,
}

/// The stream is finished. `active_order_id` is the order this stream
/// actually read from/appended to — pass it back as `order` for stable
/// pagination. `canonical_order_id` is the query's current best-known
/// relevance order, if one has ever been published. `next_cursor`/
/// `has_more` describe this subscriber's own window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDone {
    pub active_order_id: i64,
    pub canonical_order_id: Option<i64>,
    pub next_cursor: usize,
    pub has_more: bool,
}

/// A terminal failure — either a genuine cache/build error, or (via
/// [`build_stream`]'s accumulator) the streaming equivalent of `search()`'s
/// `AllEnginesFailed`/`AllEnginesCoolingDown`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamErrorPayload {
    pub message: String,
}

/// One serializable event in a search stream. Serialized untagged: the wire
/// payload is exactly the inner struct, with no enum discriminant — pair it
/// with [`StreamEvent::name`] for the SSE `event:` field.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum StreamEvent<T> {
    Meta(StreamMeta),
    Results(StreamResults<T>),
    Attribution(StreamAttribution),
    Engine(EngineReport),
    Done(StreamDone),
    Error(StreamErrorPayload),
}

impl<T> StreamEvent<T> {
    /// The SSE `event:` name this frame should be sent under.
    pub fn name(&self) -> &'static str {
        match self {
            StreamEvent::Meta(_) => "meta",
            StreamEvent::Results(_) => "results",
            StreamEvent::Attribution(_) => "attribution",
            StreamEvent::Engine(_) => "engine",
            StreamEvent::Done(_) => "done",
            StreamEvent::Error(_) => "error",
        }
    }
}

/// A [`StreamEvent`] tagged with its position in the underlying cache
/// build's sequence, when it has one. Cooling-down `engine` reports emitted
/// before the cache subscription even starts have no cache sequence, so
/// `sequence` is `None` for those; every cache-derived event preserves its
/// exact cache sequence id unchanged, so `after_sequence` reconnect keeps
/// working end to end.
#[derive(Debug, Clone)]
pub struct SequencedStreamEvent<T> {
    pub sequence: Option<u64>,
    pub event: StreamEvent<T>,
}

/// The result of [`SearchBuilder::stream`]/[`ImageSearchBuilder::stream`]:
/// an already-available snapshot plus a live channel for events not yet
/// available — mirrors [`CacheSubscription`] one layer up, in the
/// browser-facing event shape.
pub struct SearchStream<T> {
    pub snapshot: Vec<SequencedStreamEvent<T>>,
    pub live: mpsc::UnboundedReceiver<SequencedStreamEvent<T>>,
}

/// A query word matching a whole `.`/`-`-delimited domain segment (e.g.
/// "rust" in `rust-lang.org`) is a strong, deliberate signal — someone
/// searching "rust" almost certainly wants the Rust site itself first.
const DOMAIN_TOKEN_MATCH: u32 = 3;
/// A word merely appearing *somewhere* in the domain, not aligned to a
/// segment boundary (e.g. "rust" inside `trustworthy.com`), is a much
/// weaker, false-positive-prone signal — still worth something, but must
/// never outrank an actual word-boundary domain match.
const DOMAIN_SUBSTRING_MATCH: u32 = 2;
/// A word appearing in the path rather than the domain (e.g.
/// `example.com/blog/rust-tutorial`) is a real but weak match — the site
/// itself isn't about the query, just one page on it happens to mention it.
const PATH_MATCH: u32 = 1;

fn query_words(query: &str) -> Vec<String> {
    let stop = ["the", "and", "or", "of", "for", "in", "on", "at"];
    query
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| !character.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty() && !stop.contains(&word.as_str()))
        .collect()
}

fn url_score(url: &str, words: &[String]) -> u32 {
    let rest = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (domain, path) = rest.split_once('/').unwrap_or((rest, ""));
    let domain = domain.to_lowercase();
    let path = path.to_lowercase();

    let domain_tokens: Vec<&str> = domain
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();

    words
        .iter()
        .map(|word| {
            if domain_tokens.contains(&word.as_str()) {
                DOMAIN_TOKEN_MATCH
            } else if domain.contains(word.as_str()) {
                DOMAIN_SUBSTRING_MATCH
            } else if path.contains(word.as_str()) {
                PATH_MATCH
            } else {
                0
            }
        })
        .sum()
}

/// Ranks a freshly-fetched batch by how closely each result's URL matches
/// the (non-stopword) query terms. This generic URL-only form remains public
/// for consumers with their own row type; the built-in text ranker below adds
/// title/description and programmer-site signals as well.
pub fn sort_results<T: CacheableRow>(mut results: Vec<T>, query: &str) -> Vec<T> {
    let words = query_words(query);
    results.sort_by_cached_key(|result| std::cmp::Reverse(url_score(result.url(), &words)));
    results
}

fn programmer_domain_boost(url: &str) -> u32 {
    let domain = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();

    match domain.as_str() {
        "github.com" => 30,
        "gitlab.com" | "codeberg.org" => 18,
        "stackoverflow.com" => 20,
        "crates.io"
        | "www.npmjs.com"
        | "pypi.org"
        | "rubygems.org"
        | "packagist.org"
        | "central.sonatype.com"
        | "www.nuget.org"
        | "hex.pm"
        | "pkg.go.dev" => 8,
        _ => 0,
    }
}

/// Shared by the ranker (which decides persisted order) and by
/// [`to_search_result`] (which decides the score on the wire). Both must pass
/// the same `programmer_boost`, or the browser would see a score that
/// disagrees with the order it arrived in.
fn cached_result_score(
    result: &CachedResult,
    query: &str,
    words: &[String],
    programmer_boost: bool,
) -> u32 {
    let title = result.title.to_lowercase();
    let description = result.description.to_lowercase();
    let title_tokens: Vec<&str> = title
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let terminal_path = result
        .url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();

    let boost = if programmer_boost {
        programmer_domain_boost(&result.url)
    } else {
        0
    };
    let mut score = url_score(&result.url, words) * 10 + boost;
    if title.trim() == query.trim().to_lowercase() {
        score += 20;
    }
    for word in words {
        if title_tokens.contains(&word.as_str()) {
            score += 6;
        } else if title.contains(word) {
            score += 3;
        }
        if description.contains(word) {
            score += 1;
        }
        if terminal_path.eq_ignore_ascii_case(word) {
            score += 5;
        }
    }
    score
}

struct DomainWordRanker {
    programmer_boost: bool,
}

impl Ranker<CachedResult> for DomainWordRanker {
    fn rank(&self, query: &str, mut batch: Vec<CachedResult>) -> Vec<CachedResult> {
        let words = query_words(query);
        batch.sort_by_cached_key(|result| {
            std::cmp::Reverse(cached_result_score(
                result,
                query,
                &words,
                self.programmer_boost,
            ))
        });
        batch
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

/// Adapts any [`SearchEngine`] into the cache layer's [`EngineSource`].
///
/// Was one hand-written source struct per engine; with twenty-odd adapters
/// that became twenty identical copies of the same six-line mapping, so the
/// conversion lives here once instead.
struct TextSource<E> {
    engine: E,
}

fn source_error(error: search_engines::EngineError) -> EngineOutcome {
    match error {
        search_engines::EngineError::PaginationExpired(message) => EngineOutcome::Deferred(message),
        error => EngineOutcome::Failed(error.to_string()),
    }
}

#[async_trait]
impl<E> EngineSource<CachedResult> for TextSource<E>
where
    E: SearchEngine + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.engine.name()
    }

    fn next_start(&self, start: usize, result_count: usize) -> usize {
        self.engine.next_start(start, result_count)
    }

    async fn wait_ready(&self) -> Result<(), String> {
        wait_for_engine(self.name()).await
    }

    async fn fetch_page(
        &self,
        query: &str,
        start: usize,
    ) -> Result<Vec<CachedResult>, EngineOutcome> {
        self.engine
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
            .map_err(source_error)
    }
}

/// [`TextSource`]'s counterpart for image engines.
struct ImageSource<E>(E);

#[async_trait]
impl<E> EngineSource<CachedImage> for ImageSource<E>
where
    E: ImageEngine + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.0.name()
    }

    async fn wait_ready(&self) -> Result<(), String> {
        wait_for_engine(self.name()).await
    }

    async fn fetch_page(
        &self,
        query: &str,
        start: usize,
    ) -> Result<Vec<CachedImage>, EngineOutcome> {
        self.0
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
            .map_err(source_error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEngines {
    Brave,
    DuckDuckGo,
    GitHub,
    GitLab,
    Codeberg,
    StackExchange,
    HackerNews,
    Lobsters,
    Wikipedia,
    ArchWiki,
    Mdn,
    CratesIo,
    Npm,
    PyPi,
    RubyGems,
    Packagist,
    MavenCentral,
    NuGet,
    HexPm,
    DockerHub,
    NixPackages,
    GoPkg,
}

impl SearchEngines {
    /// Every known text-search engine, including quota-sensitive sources.
    pub fn all() -> Vec<Self> {
        vec![
            Self::Brave,
            Self::DuckDuckGo,
            Self::GitHub,
            Self::GitLab,
            Self::Codeberg,
            Self::StackExchange,
            Self::HackerNews,
            Self::Lobsters,
            Self::Wikipedia,
            Self::ArchWiki,
            Self::Mdn,
            Self::CratesIo,
            Self::Npm,
            Self::PyPi,
            Self::RubyGems,
            Self::Packagist,
            Self::MavenCentral,
            Self::NuGet,
            Self::HexPm,
            Self::DockerHub,
            Self::NixPackages,
            Self::GoPkg,
        ]
    }

    fn name(self) -> &'static str {
        match self {
            Self::Brave => Brave.name(),
            Self::DuckDuckGo => DuckDuckGo.name(),
            Self::GitHub => GitHub.name(),
            Self::GitLab => GitLab.name(),
            Self::Codeberg => Codeberg.name(),
            Self::StackExchange => StackExchange.name(),
            Self::HackerNews => HackerNews.name(),
            Self::Lobsters => Lobsters.name(),
            Self::Wikipedia => Wikipedia.name(),
            Self::ArchWiki => ArchWiki.name(),
            Self::Mdn => Mdn.name(),
            Self::CratesIo => CratesIo.name(),
            Self::Npm => Npm.name(),
            Self::PyPi => PyPi.name(),
            Self::RubyGems => RubyGems.name(),
            Self::Packagist => Packagist.name(),
            Self::MavenCentral => MavenCentral.name(),
            Self::NuGet => NuGet.name(),
            Self::HexPm => HexPm.name(),
            Self::DockerHub => DockerHub.name(),
            Self::NixPackages => NixPackages.name(),
            Self::GoPkg => GoPkg.name(),
        }
    }

    fn source(self) -> Arc<dyn EngineSource<CachedResult>> {
        macro_rules! source {
            ($engine:expr) => {
                Arc::new(TextSource { engine: $engine })
            };
        }
        match self {
            Self::Brave => source!(Brave),
            Self::DuckDuckGo => source!(DuckDuckGo),
            Self::GitHub => source!(GitHub),
            Self::GitLab => source!(GitLab),
            Self::Codeberg => source!(Codeberg),
            Self::StackExchange => source!(StackExchange),
            Self::HackerNews => source!(HackerNews),
            Self::Lobsters => source!(Lobsters),
            Self::Wikipedia => source!(Wikipedia),
            Self::ArchWiki => source!(ArchWiki),
            Self::Mdn => source!(Mdn),
            Self::CratesIo => source!(CratesIo),
            Self::Npm => source!(Npm),
            Self::PyPi => source!(PyPi),
            Self::RubyGems => source!(RubyGems),
            Self::Packagist => source!(Packagist),
            Self::MavenCentral => source!(MavenCentral),
            Self::NuGet => source!(NuGet),
            Self::HexPm => source!(HexPm),
            Self::DockerHub => source!(DockerHub),
            Self::NixPackages => source!(NixPackages),
            Self::GoPkg => source!(GoPkg),
        }
    }
}

/// Which category of search the caller is running. The tab decides the
/// engine set, the cache namespace, and whether programmer-domain scoring
/// applies — no query-text guessing, which is what used to drop GitHub and
/// MDN results into ordinary web searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchProfile {
    #[default]
    General,
    Tech,
    Programming,
}

impl SearchProfile {
    /// Fixed per-profile fanout. Deliberately narrow: fewer engines per
    /// category means less traffic per engine and so fewer bot-wall bans.
    pub fn engines(self) -> Vec<SearchEngines> {
        match self {
            Self::General => vec![
                SearchEngines::Brave,
                SearchEngines::DuckDuckGo,
                SearchEngines::Wikipedia,
            ],
            Self::Tech => vec![SearchEngines::HackerNews, SearchEngines::Lobsters],
            Self::Programming => vec![
                SearchEngines::GitHub,
                SearchEngines::CratesIo,
                SearchEngines::GoPkg,
            ],
        }
    }

    /// Separate namespace per profile: the cache key is (namespace, query),
    /// and neither the source set nor the ranker is part of cache identity,
    /// so one shared namespace would serve Tech's rows for a General query.
    /// The legacy `text` namespace is intentionally abandoned — its stored
    /// order was built with the old routing and unconditional boosts.
    fn namespace(self) -> &'static str {
        match self {
            Self::General => "text-general",
            Self::Tech => "text-tech",
            Self::Programming => "text-programming",
        }
    }

    /// GitHub/registry domains only deserve a boost when that's what the
    /// user asked for. Applying it to General is the single biggest reason
    /// non-technical queries returned developer junk at the top.
    fn programmer_boost(self) -> bool {
        matches!(self, Self::Programming)
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
            Self::Brave => Arc::new(ImageSource(Brave)),
        }
    }
}

static GENERAL_CACHE: OnceCell<MergedCache<CachedResult>> = OnceCell::const_new();
static TECH_CACHE: OnceCell<MergedCache<CachedResult>> = OnceCell::const_new();
static PROGRAMMING_CACHE: OnceCell<MergedCache<CachedResult>> = OnceCell::const_new();

async fn text_cache(profile: SearchProfile) -> &'static MergedCache<CachedResult> {
    let cell = match profile {
        SearchProfile::General => &GENERAL_CACHE,
        SearchProfile::Tech => &TECH_CACHE,
        SearchProfile::Programming => &PROGRAMMING_CACHE,
    };
    cell.get_or_init(|| async {
        let pool = search_cache::shared_pool().await.clone();
        MergedCache::new(
            pool,
            profile.namespace(),
            Arc::new(DomainWordRanker {
                programmer_boost: profile.programmer_boost(),
            }),
        )
        // One bounded server-side hold replaces the browser's unreliable
        // 1.2s buffer. All usable engines in the selected profile are in the
        // first round, so they are the first-window primaries; cooling engines
        // never enter `sources` and therefore cannot hold this open.
        .with_first_window_hold(Duration::from_millis(2_500))
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
    !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|(_, o)| !matches!(o, EngineOutcome::Ok))
}

/// Splits `engines` into those usable right now and those still cooling
/// down (paired with their remaining cooldown). Shared by
/// `SearchBuilder`/`ImageSearchBuilder` since the cooldown registry is keyed
/// purely by engine name, independent of which enum (`SearchEngines` or
/// `ImageEngines`) the caller happens to use.
fn partition_by_cooldown<E: Copy>(
    engines: &[E],
    name_of: impl Fn(E) -> &'static str,
) -> (Vec<E>, Vec<(E, Duration)>) {
    let mut usable = Vec::new();
    let mut cooling = Vec::new();
    for &e in engines {
        match cooldown_remaining(name_of(e)) {
            Some(remaining) => cooling.push((e, remaining)),
            None => usable.push(e),
        }
    }
    (usable, cooling)
}

/// Feeds this call's outcomes back into the cooldown registry: a success
/// clears any cooldown, a failure/timeout starts or extends one. Only
/// `usable` engines can have an outcome at all — a cooling engine was never
/// contacted this call, so it has nothing to record.
fn record_outcomes<E: Copy>(
    usable: &[E],
    name_of: impl Fn(E) -> &'static str,
    outcomes: &[(String, EngineOutcome)],
) {
    for &e in usable {
        let name = name_of(e);
        if let Some((_, outcome)) = outcomes.iter().rev().find(|(n, _)| n == name) {
            match outcome {
                EngineOutcome::Ok => record_success(name),
                EngineOutcome::Failed(_) | EngineOutcome::TimedOut => record_failure(name),
                EngineOutcome::Deferred(_) => {}
            }
        }
    }
}

/// Builds one `EngineReport` per requested engine, in the original request
/// order, so a cooling engine still shows up in the response (as
/// `EngineStatus::CoolingDown`) instead of silently vanishing from it —
/// callers need to know an engine was deliberately skipped, not just absent.
fn build_reports<E: Copy + PartialEq>(
    engines: &[E],
    name_of: impl Fn(E) -> &'static str,
    cooling: &[(E, Duration)],
    outcomes: &[(String, EngineOutcome)],
) -> Vec<EngineReport> {
    engines
        .iter()
        .map(|&e| {
            let name = name_of(e);
            if let Some((_, remaining)) = cooling.iter().find(|(ce, _)| *ce == e) {
                EngineReport {
                    engine: name.to_string(),
                    status: EngineStatus::CoolingDown(format!("retry in {}s", remaining.as_secs())),
                }
            } else {
                let status = outcomes
                    .iter()
                    .rev()
                    .find(|(n, _)| n == name)
                    .map(|(_, o)| EngineStatus::from(o))
                    .unwrap_or(EngineStatus::Ok);
                EngineReport {
                    engine: name.to_string(),
                    status,
                }
            }
        })
        .collect()
}

/// Builds id-less `EngineReport`s for engines skipped this call because
/// they're still cooling down from a recent failure — meant to be emitted up
/// front, before the cache snapshot, so a stream subscriber learns about a
/// skipped engine immediately instead of waiting for a terminal event.
fn cooling_reports<E: Copy>(
    cooling: &[(E, Duration)],
    name_of: impl Fn(E) -> &'static str,
) -> Vec<EngineReport> {
    cooling
        .iter()
        .map(|(e, remaining)| EngineReport {
            engine: name_of(*e).to_string(),
            status: EngineStatus::CoolingDown(format!("retry in {}s", remaining.as_secs())),
        })
        .collect()
}

/// Turns one raw cache event into its serializable stream form. Metadata,
/// Attribution, Engine, and Done/Error map 1:1 (Done/Error substitution for
/// `AllEnginesFailed`/`AllEnginesCoolingDown`-equivalent semantics is decided
/// by the caller, in [`build_stream`], since it needs cross-event state this
/// function doesn't have); Results entries are converted from the merge
/// cache's row type via `convert`.
fn map_cache_event<R: CacheableRow, T, C>(
    query: &str,
    event: CacheEvent<R>,
    convert: C,
) -> StreamEvent<T>
where
    C: Fn(&str, MergedRowResult<R>) -> T,
{
    match event {
        CacheEvent::Metadata {
            order_id,
            order_kind,
            cached,
        } => StreamEvent::Meta(StreamMeta {
            order_id,
            canonical: matches!(order_kind, OrderKind::Canonical),
            cached,
        }),
        CacheEvent::Results(rows) => StreamEvent::Results(StreamResults {
            entries: rows
                .into_iter()
                .map(|pr| PositionedResult {
                    position: pr.position,
                    result: convert(query, pr.row),
                })
                .collect(),
        }),
        CacheEvent::Attribution {
            position,
            url,
            engines,
        } => StreamEvent::Attribution(StreamAttribution {
            position,
            url,
            engines,
        }),
        CacheEvent::Engine { engine, outcome } => StreamEvent::Engine(EngineReport {
            engine,
            status: EngineStatus::from(&outcome),
        }),
        CacheEvent::Done {
            active_order,
            canonical_order,
            next_cursor,
            has_more,
        } => StreamEvent::Done(StreamDone {
            active_order_id: active_order,
            canonical_order_id: canonical_order,
            next_cursor,
            has_more,
        }),
        CacheEvent::Error(message) => StreamEvent::Error(StreamErrorPayload { message }),
    }
}

/// Accumulates just enough per-subscriber state, across snapshot and live
/// events, to answer the same question `search()` answers only after
/// collecting everything: is this window actually empty, and if so, is that
/// because nothing was usable, or because everything contacted failed? The
/// streaming equivalent of `search()`'s post-hoc
/// `rows.is_empty() && (usable.is_empty() | all_contacted_engines_failed(..))`
/// check, evaluated incrementally instead of after a full collect.
#[derive(Default)]
struct StreamAccumulator {
    any_results: bool,
    temporarily_unavailable: bool,
    outcomes: Vec<(String, EngineOutcome)>,
}

impl StreamAccumulator {
    fn observe<R>(&mut self, event: &CacheEvent<R>) {
        match event {
            CacheEvent::Results(rows) if !rows.is_empty() => self.any_results = true,
            CacheEvent::Engine { engine, outcome } => {
                self.outcomes.push((engine.clone(), outcome.clone()));
            }
            _ => {}
        }
    }

    /// `Some(message)` if a `Done` reached right now should really be
    /// surfaced as an application error instead (mirroring
    /// `FetchError::AllEnginesCoolingDown`/`AllEnginesFailed`'s `Display`
    /// text, for a client-facing message consistent with `/query`'s).
    fn terminal_error_message(&self, usable_is_empty: bool) -> Option<String> {
        if self.any_results {
            return None;
        }
        if usable_is_empty {
            Some(FetchError::AllEnginesCoolingDown.to_string())
        } else if all_contacted_engines_failed(&self.outcomes) {
            Some(FetchError::AllEnginesFailed.to_string())
        } else {
            None
        }
    }
}

/// Maps one `is_done`/`is_error`-aware event through `acc`, substituting an
/// `error` frame for what would otherwise be an empty-window `Done`. Shared
/// between the synchronous snapshot pass and the spawned live-forwarding
/// task in [`build_stream`] so the substitution logic can't drift between
/// the two.
fn map_and_observe<R: CacheableRow, T, C>(
    acc: &mut StreamAccumulator,
    usable_is_empty: bool,
    query: &str,
    event: CacheEvent<R>,
    convert: C,
) -> (StreamEvent<T>, bool)
where
    C: Fn(&str, MergedRowResult<R>) -> T + Copy,
{
    acc.observe(&event);
    let is_done = matches!(event, CacheEvent::Done { .. });
    let is_terminal = is_done || matches!(event, CacheEvent::Error(_));
    let mut mapped = if is_done {
        match acc.terminal_error_message(usable_is_empty) {
            Some(message) => StreamEvent::Error(StreamErrorPayload { message }),
            None => map_cache_event(query, event, convert),
        }
    } else {
        map_cache_event(query, event, convert)
    };
    if let StreamEvent::Done(done) = &mut mapped {
        done.has_more |= acc.temporarily_unavailable;
    }
    (mapped, is_terminal)
}

/// Turns a raw [`CacheSubscription`] into a browser-facing [`SearchStream`]:
/// maps every event (snapshot synchronously, live via one spawned forwarding
/// task — genuinely incremental, never collected into a `Vec` first),
/// prepends id-less cooling-down `engine` reports, substitutes an `error`
/// frame for an empty-window `Done`, and — only for the build's owner (see
/// [`CacheSubscription::is_owner`]) — calls `record` exactly once with the
/// final accumulated outcomes when a terminal event is reached. The
/// forwarding task keeps draining `live` (ignoring send errors) even if the
/// outbound receiver is dropped, so the owner's outcomes are still recorded
/// even if the HTTP-side subscriber disconnects first.
fn build_stream<R, T, F, C>(
    query: String,
    subscription: CacheSubscription<R>,
    cooling_reports: Vec<EngineReport>,
    usable_is_empty: bool,
    convert: C,
    record: F,
) -> SearchStream<T>
where
    R: CacheableRow,
    T: Send + 'static,
    F: FnOnce(&[(String, EngineOutcome)]) + Send + 'static,
    C: Fn(&str, MergedRowResult<R>) -> T + Copy + Send + 'static,
{
    let CacheSubscription {
        snapshot,
        live,
        is_owner,
    } = subscription;

    let temporarily_unavailable = !cooling_reports.is_empty();
    let mut out_snapshot: Vec<SequencedStreamEvent<T>> = cooling_reports
        .into_iter()
        .map(|report| SequencedStreamEvent {
            sequence: None,
            event: StreamEvent::Engine(report),
        })
        .collect();

    let mut acc = StreamAccumulator {
        temporarily_unavailable,
        ..StreamAccumulator::default()
    };
    let mut terminal = false;

    for seq_event in snapshot {
        let SequencedCacheEvent { sequence, event } = seq_event;
        let (mapped, is_terminal) =
            map_and_observe(&mut acc, usable_is_empty, &query, event, convert);
        out_snapshot.push(SequencedStreamEvent {
            sequence: Some(sequence),
            event: mapped,
        });
        if is_terminal {
            terminal = true;
            break;
        }
    }

    if terminal {
        // Provably unreachable when `is_owner` is true: an owner's own
        // snapshot is taken strictly before its build is spawned, so it can
        // never contain a terminal (or any) cache-derived event. Handled
        // anyway rather than assumed away.
        if is_owner {
            record(&acc.outcomes);
        }
        let (_tx, live_out) = mpsc::unbounded_channel();
        return SearchStream {
            snapshot: out_snapshot,
            live: live_out,
        };
    }

    let (tx, live_out) = mpsc::unbounded_channel::<SequencedStreamEvent<T>>();
    tokio::spawn(async move {
        let mut live = live;
        let mut acc = acc;
        while let Some(seq_event) = live.recv().await {
            let SequencedCacheEvent { sequence, event } = seq_event;
            let (mapped, is_terminal) =
                map_and_observe(&mut acc, usable_is_empty, &query, event, convert);
            // Ignore send errors: keep draining so the owner's outcomes are
            // still recorded exactly once even if the downstream (HTTP)
            // receiver disconnected mid-stream.
            let _ = tx.send(SequencedStreamEvent {
                sequence: Some(sequence),
                event: mapped,
            });
            if is_terminal {
                if is_owner {
                    record(&acc.outcomes);
                }
                return;
            }
        }
    });

    SearchStream {
        snapshot: out_snapshot,
        live: live_out,
    }
}

/// Builds and runs a text search across one or more engines.
///
/// Defaults: the quota-aware engine set, 10 results from 0, and an 8s timeout (configurable with `ENGINE_TIMEOUT_SECS`).
pub struct SearchBuilder {
    query: String,
    profile: SearchProfile,
    engines: Vec<SearchEngines>,
    start: usize,
    count: usize,
    timeout: Duration,
    order_token: Option<i64>,
    after_sequence: Option<u64>,
}

impl SearchBuilder {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            profile: SearchProfile::default(),
            engines: Vec::new(),
            start: 0,
            count: DEFAULT_SEARCH_COUNT,
            timeout: default_engine_timeout(),
            order_token: None,
            after_sequence: None,
        }
    }

    /// Which category this search belongs to. Picks the default engine set,
    /// the cache namespace, and the scoring policy. An explicit
    /// [`engine`](Self::engine)/[`engines`](Self::engines) call overrides only
    /// the engine set — namespace and scoring still follow the profile, so a
    /// hand-picked engine list can never contaminate another tab's cache.
    pub fn profile(mut self, profile: SearchProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Adds an engine to query; duplicates are ignored. Defaults to the
    /// profile's engine set if never called.
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

    /// Per-engine, per-round timeout. Default 8 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Continues pagination against a specific order token from an earlier
    /// stream's `done` event, instead of the query's current canonical
    /// order. Only meaningful for [`Self::stream`]; `.search()` never sets
    /// one.
    pub fn order_token(mut self, order_token: Option<i64>) -> Self {
        self.order_token = order_token;
        self
    }

    /// Resumes a [`Self::stream`] after this cache sequence number, for
    /// reconnect. Only meaningful for [`Self::stream`].
    pub fn after_sequence(mut self, after_sequence: Option<u64>) -> Self {
        self.after_sequence = after_sequence;
        self
    }

    /// Runs the search, extending the merged cache as needed and ranking any
    /// newly-discovered results by [`sort_results`].
    pub async fn search(self) -> Result<SearchResponse<SearchResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            self.profile.engines()
        } else {
            self.engines
        };

        let (usable, cooling) = partition_by_cooldown(&engines, SearchEngines::name);

        // Even with zero sources, `get_or_extend` can still serve rows
        // already in the merged cache — that's the main benefit of cooling
        // down an engine instead of just erroring out, so this call is never
        // skipped even when every engine is currently cooling.
        let sources: Vec<Arc<dyn EngineSource<CachedResult>>> =
            usable.iter().map(|e| e.source()).collect();

        let extend = text_cache(self.profile)
            .await
            .get_or_extend(&self.query, &sources, self.start, self.count, self.timeout)
            .await?;

        // Guards against double-recording cooldown outcomes when a
        // concurrent identical call joined this same underlying cache
        // build: only the call that actually spawned/owns the build records
        // — a join sees the exact same `Engine` events its owner already
        // will.
        if extend.is_owner {
            record_outcomes(&usable, SearchEngines::name, &extend.engine_outcomes);
        }

        if extend.rows.is_empty() {
            if usable.is_empty() {
                return Err(FetchError::AllEnginesCoolingDown);
            }
            if all_contacted_engines_failed(&extend.engine_outcomes) {
                return Err(FetchError::AllEnginesFailed);
            }
        }

        let reports = build_reports(
            &engines,
            SearchEngines::name,
            &cooling,
            &extend.engine_outcomes,
        );

        let results = extend
            .rows
            .into_iter()
            .map(|r| to_search_result(self.profile, &self.query, r))
            .collect();

        Ok(SearchResponse {
            results,
            engines: reports,
            has_more: extend.has_more || !cooling.is_empty(),
        })
    }

    /// Streaming equivalent of [`Self::search`]: returns a snapshot of
    /// whatever's already known plus a live channel for events not yet
    /// available, instead of waiting for everything to settle. Reuses the
    /// exact same engine selection, cooldown partitioning, and source
    /// adapters as `.search()` — only the cache call (`subscribe_or_extend`
    /// instead of `get_or_extend`) and the outbound event shape differ.
    pub async fn stream(self) -> Result<SearchStream<SearchResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            self.profile.engines()
        } else {
            self.engines
        };

        let (usable, cooling) = partition_by_cooldown(&engines, SearchEngines::name);
        let sources: Vec<Arc<dyn EngineSource<CachedResult>>> =
            usable.iter().map(|e| e.source()).collect();
        let cooling_reports = cooling_reports(&cooling, SearchEngines::name);

        let subscription = text_cache(self.profile)
            .await
            .subscribe_or_extend(
                &self.query,
                sources,
                self.start,
                self.count,
                self.order_token,
                self.timeout,
                self.after_sequence,
            )
            .await?;

        let usable_is_empty = usable.is_empty();
        let record_usable = usable.clone();
        let profile = self.profile;
        let record = move |outcomes: &[(String, EngineOutcome)]| {
            record_outcomes(&record_usable, SearchEngines::name, outcomes);
        };

        Ok(build_stream(
            self.query,
            subscription,
            cooling_reports,
            usable_is_empty,
            move |query: &str, row| to_search_result(profile, query, row),
            record,
        ))
    }
}

fn to_search_result(
    profile: SearchProfile,
    query: &str,
    row: MergedRowResult<CachedResult>,
) -> SearchResult {
    let words = query_words(query);
    let score = cached_result_score(&row.value, query, &words, profile.programmer_boost());
    SearchResult {
        url: row.value.url,
        title: row.value.title,
        description: row.value.description,
        engines: row.engines,
        cached: row.cached,
        score,
    }
}

fn to_image_result(_query: &str, row: MergedRowResult<CachedImage>) -> ImageResult {
    ImageResult {
        url: row.value.url,
        title: row.value.title,
        engines: row.engines,
        cached: row.cached,
    }
}

/// Builds and runs an image search across one or more engines.
///
/// Defaults: every image engine, 50 results from 0, and an 8s timeout (configurable with `ENGINE_TIMEOUT_SECS`).
pub struct ImageSearchBuilder {
    query: String,
    engines: Vec<ImageEngines>,
    start: usize,
    count: usize,
    timeout: Duration,
    order_token: Option<i64>,
    after_sequence: Option<u64>,
}

impl ImageSearchBuilder {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            engines: Vec::new(),
            start: 0,
            count: DEFAULT_IMAGE_COUNT,
            timeout: default_engine_timeout(),
            order_token: None,
            after_sequence: None,
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

    /// Per-engine, per-round timeout. Default 8 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Continues pagination against a specific order token from an earlier
    /// stream's `done` event, instead of the query's current canonical
    /// order. Only meaningful for [`Self::stream`]; `.search()` never sets
    /// one.
    pub fn order_token(mut self, order_token: Option<i64>) -> Self {
        self.order_token = order_token;
        self
    }

    /// Resumes a [`Self::stream`] after this cache sequence number, for
    /// reconnect. Only meaningful for [`Self::stream`].
    pub fn after_sequence(mut self, after_sequence: Option<u64>) -> Self {
        self.after_sequence = after_sequence;
        self
    }

    /// Runs the search, extending the merged cache as needed.
    pub async fn search(self) -> Result<SearchResponse<ImageResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            ImageEngines::all()
        } else {
            self.engines
        };

        let (usable, cooling) = partition_by_cooldown(&engines, ImageEngines::name);

        // Even with zero sources, `get_or_extend` can still serve rows
        // already in the merged cache — that's the main benefit of cooling
        // down an engine instead of just erroring out, so this call is never
        // skipped even when every engine is currently cooling.
        let sources: Vec<Arc<dyn EngineSource<CachedImage>>> =
            usable.iter().map(|e| e.source()).collect();

        let extend = image_cache()
            .await
            .get_or_extend(&self.query, &sources, self.start, self.count, self.timeout)
            .await?;

        // See the matching comment in `SearchBuilder::search`.
        if extend.is_owner {
            record_outcomes(&usable, ImageEngines::name, &extend.engine_outcomes);
        }

        if extend.rows.is_empty() {
            if usable.is_empty() {
                return Err(FetchError::AllEnginesCoolingDown);
            }
            if all_contacted_engines_failed(&extend.engine_outcomes) {
                return Err(FetchError::AllEnginesFailed);
            }
        }

        let reports = build_reports(
            &engines,
            ImageEngines::name,
            &cooling,
            &extend.engine_outcomes,
        );

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
            has_more: extend.has_more || !cooling.is_empty(),
        })
    }

    /// Streaming equivalent of [`Self::search`]; see
    /// [`SearchBuilder::stream`] for the shared design notes.
    pub async fn stream(self) -> Result<SearchStream<ImageResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            ImageEngines::all()
        } else {
            self.engines
        };

        let (usable, cooling) = partition_by_cooldown(&engines, ImageEngines::name);
        let sources: Vec<Arc<dyn EngineSource<CachedImage>>> =
            usable.iter().map(|e| e.source()).collect();
        let cooling_reports = cooling_reports(&cooling, ImageEngines::name);

        let subscription = image_cache()
            .await
            .subscribe_or_extend(
                &self.query,
                sources,
                self.start,
                self.count,
                self.order_token,
                self.timeout,
                self.after_sequence,
            )
            .await?;

        let usable_is_empty = usable.is_empty();
        let record_usable = usable.clone();
        let record = move |outcomes: &[(String, EngineOutcome)]| {
            record_outcomes(&record_usable, ImageEngines::name, outcomes);
        };

        Ok(build_stream(
            self.query,
            subscription,
            cooling_reports,
            usable_is_empty,
            to_image_result,
            record,
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use search_cache::PositionedRow;

    #[tokio::test]
    async fn requests_share_pacing_and_queued_work_rechecks_cooldown() {
        let name = "paced-test-engine";
        let gate = tokio::sync::Mutex::new(tokio::time::Instant::now());
        let interval = Duration::from_millis(20);
        let request = || async {
            reserve_engine_slot(&gate, name, interval).await.unwrap();
            tokio::time::Instant::now()
        };
        let (first, second, third) = tokio::join!(request(), request(), request());
        assert!(second.duration_since(first) >= interval - Duration::from_millis(1));
        assert!(third.duration_since(second) >= interval - Duration::from_millis(1));
        record_failure(name);
        assert!(reserve_engine_slot(&gate, name, interval).await.is_err());
        record_success(name);
    }

    #[test]
    fn later_page_failure_is_not_hidden_by_first_page_success() {
        let name = "later-page-failure-test";
        let outcomes = vec![
            (name.into(), EngineOutcome::Ok),
            (name.into(), EngineOutcome::Failed("blocked".into())),
        ];
        record_outcomes(&[name], |name| name, &outcomes);
        assert!(cooldown_remaining(name).is_some());
        let reports = build_reports(&[name], |name| name, &[], &outcomes);
        assert!(matches!(reports[0].status, EngineStatus::Failed(_)));
        record_success(name);
    }

    #[test]
    fn expired_pagination_does_not_disable_the_engine() {
        let name = "expired-pagination-test";
        let outcome = source_error(search_engines::EngineError::PaginationExpired(
            "expired".into(),
        ));
        assert!(matches!(outcome, EngineOutcome::Deferred(_)));
        record_outcomes(&[name], |name| name, &[(name.into(), outcome)]);
        assert!(cooldown_remaining(name).is_none());
    }

    #[test]
    fn cached_results_with_cooling_sources_do_not_claim_exhaustion() {
        let mut acc = StreamAccumulator {
            any_results: true,
            temporarily_unavailable: true,
            ..StreamAccumulator::default()
        };
        let (event, _) = map_and_observe(&mut acc, true, "q", done_event(1), convert_for_test);
        assert!(matches!(
            event,
            StreamEvent::Done(StreamDone { has_more: true, .. })
        ));
    }

    /// Tests exercise the stream/mapping plumbing, not scoring policy, so
    /// they all go through one profile.
    fn convert_for_test(query: &str, row: MergedRowResult<CachedResult>) -> SearchResult {
        to_search_result(SearchProfile::General, query, row)
    }

    #[test]
    fn all_text_engines_have_unique_names() {
        let engines = SearchEngines::all();
        assert_eq!(engines.len(), 22);

        let mut names: Vec<_> = engines.iter().map(|engine| engine.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), engines.len());
    }

    #[test]
    fn each_profile_has_a_fixed_engine_set() {
        assert_eq!(
            SearchProfile::General.engines(),
            vec![
                SearchEngines::Brave,
                SearchEngines::DuckDuckGo,
                SearchEngines::Wikipedia,
            ]
        );
        assert_eq!(
            SearchProfile::Tech.engines(),
            vec![SearchEngines::HackerNews, SearchEngines::Lobsters]
        );
        assert_eq!(
            SearchProfile::Programming.engines(),
            vec![
                SearchEngines::GitHub,
                SearchEngines::CratesIo,
                SearchEngines::GoPkg,
            ]
        );
    }

    /// The whole point of explicit tabs: query wording used to silently
    /// reroute the fanout ("go", "rust", "container" all triggered package
    /// registries), which is what put developer results in front of people
    /// searching for anything else.
    #[test]
    fn profile_engines_do_not_depend_on_query_wording() {
        let baseline = SearchProfile::General.engines();
        for _ in [
            "cats",
            "ripgrep github",
            "express npm",
            "why does rust fail?",
        ] {
            assert_eq!(SearchProfile::General.engines(), baseline);
        }
    }

    #[test]
    fn general_and_tech_omit_the_programmer_domain_boost() {
        assert!(!SearchProfile::General.programmer_boost());
        assert!(!SearchProfile::Tech.programmer_boost());
        assert!(SearchProfile::Programming.programmer_boost());
    }

    #[test]
    fn each_profile_gets_its_own_cache_namespace() {
        assert_eq!(SearchProfile::General.namespace(), "text-general");
        assert_eq!(SearchProfile::Tech.namespace(), "text-tech");
        assert_eq!(SearchProfile::Programming.namespace(), "text-programming");
    }

    // The cooldown registry is process-global, and Rust runs `#[test]`s
    // concurrently in one process, so every test below uses its own
    // never-reused engine-name key rather than a shared name or relying on
    // env vars differing per test — otherwise these tests would race each
    // other through shared state.

    #[test]
    fn cooldown_remaining_is_none_for_an_engine_that_has_not_failed() {
        assert!(cooldown_remaining("test-engine-1").is_none());
    }

    #[test]
    fn record_failure_puts_an_engine_into_cooldown() {
        record_failure("test-engine-2");

        let remaining = cooldown_remaining("test-engine-2");
        assert!(remaining.is_some());
        assert!(remaining.unwrap() > Duration::ZERO);
    }

    #[test]
    fn record_success_clears_an_existing_cooldown() {
        record_failure("test-engine-3");
        assert!(cooldown_remaining("test-engine-3").is_some());

        record_success("test-engine-3");
        assert!(cooldown_remaining("test-engine-3").is_none());
    }

    #[test]
    fn consecutive_failures_escalate_the_cooldown_window() {
        record_failure("test-engine-4");
        let first = cooldown_remaining("test-engine-4").unwrap();

        record_failure("test-engine-4");
        let second = cooldown_remaining("test-engine-4").unwrap();

        assert!(
            second > first,
            "second consecutive failure's window ({second:?}) should exceed the first's ({first:?})"
        );
    }

    #[test]
    fn the_cooldown_window_is_capped() {
        for _ in 0..20 {
            record_failure("test-engine-5");
        }

        let remaining = cooldown_remaining("test-engine-5").unwrap();
        assert!(
            remaining <= cooldown_cap(),
            "remaining ({remaining:?}) should never exceed the configured cap ({:?})",
            cooldown_cap()
        );
    }

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

    #[test]
    fn sort_results_prefers_a_domain_word_boundary_match_over_a_same_substring_false_positive() {
        // "trustworthy.com" contains the letters "rust" but isn't about
        // Rust at all; "rust-lang.org" has "rust" as its own domain segment.
        let ranked = sort_results(
            vec![r("https://trustworthy.com/x"), r("https://rust-lang.org/x")],
            "rust",
        );

        assert_eq!(ranked[0].url, "https://rust-lang.org/x");
        assert_eq!(ranked[1].url, "https://trustworthy.com/x");
    }

    #[test]
    fn sort_results_prefers_a_domain_match_over_a_path_only_match() {
        // The site itself being about "rust" should outrank some other
        // site's blog post that merely mentions "rust" in its URL path.
        let ranked = sort_results(
            vec![
                r("https://some-blog.example/posts/rust-tips"),
                r("https://rust-lang.org/learn"),
            ],
            "rust",
        );

        assert_eq!(ranked[0].url, "https://rust-lang.org/learn");
        assert_eq!(ranked[1].url, "https://some-blog.example/posts/rust-tips");
    }

    #[test]
    fn sort_results_still_ranks_a_path_only_match_above_no_match_at_all() {
        // A path match is "a good match, not a great one" — still better
        // than a result with no signal whatsoever.
        let ranked = sort_results(
            vec![
                r("https://totally-unrelated.example/other"),
                r("https://some-blog.example/posts/rust-tips"),
            ],
            "rust",
        );

        assert_eq!(ranked[0].url, "https://some-blog.example/posts/rust-tips");
        assert_eq!(ranked[1].url, "https://totally-unrelated.example/other");
    }

    #[test]
    fn programming_ranker_surfaces_a_matching_github_repository() {
        let ranked = DomainWordRanker {
            programmer_boost: true,
        }
        .rank(
            "pangolin",
            vec![
                CachedResult {
                    url: "https://en.wikipedia.org/wiki/Pangolin".into(),
                    title: "Pangolin".into(),
                    description: "A mammal".into(),
                },
                CachedResult {
                    url: "https://pkg.go.dev/github.com/example/pangolin/logging".into(),
                    title: "github.com/example/pangolin/logging".into(),
                    description: "Go logging package".into(),
                },
                CachedResult {
                    url: "https://github.com/fosrl/pangolin".into(),
                    title: "fosrl/pangolin".into(),
                    description: "Networking platform".into(),
                },
            ],
        );

        assert_eq!(ranked[0].url, "https://github.com/fosrl/pangolin");
    }

    /// The bug this whole change exists for: outside the Programming tab,
    /// a github.com row with a weaker text match must not outrank the
    /// obviously-correct encyclopedia article.
    #[test]
    fn general_ranker_does_not_promote_github_over_a_better_text_match() {
        let rows = vec![
            CachedResult {
                url: "https://github.com/fosrl/pangolin".into(),
                title: "fosrl/pangolin".into(),
                description: "Networking platform".into(),
            },
            CachedResult {
                url: "https://en.wikipedia.org/wiki/Pangolin".into(),
                title: "Pangolin".into(),
                description: "A mammal".into(),
            },
        ];

        let ranked = DomainWordRanker {
            programmer_boost: false,
        }
        .rank("pangolin", rows);

        assert_eq!(ranked[0].url, "https://en.wikipedia.org/wiki/Pangolin");
    }

    /// The persisted order and the `score` sent to the browser are computed
    /// in two different places; if they ever disagree the client sees rows
    /// whose scores descend out of order.
    #[test]
    fn wire_score_agrees_with_ranked_order_for_every_profile() {
        let rows = vec![
            CachedResult {
                url: "https://github.com/fosrl/pangolin".into(),
                title: "fosrl/pangolin".into(),
                description: "Networking platform".into(),
            },
            CachedResult {
                url: "https://en.wikipedia.org/wiki/Pangolin".into(),
                title: "Pangolin".into(),
                description: "A mammal".into(),
            },
        ];

        for profile in [
            SearchProfile::General,
            SearchProfile::Tech,
            SearchProfile::Programming,
        ] {
            let ranked = DomainWordRanker {
                programmer_boost: profile.programmer_boost(),
            }
            .rank("pangolin", rows.clone());

            let scores: Vec<u32> = ranked
                .into_iter()
                .map(|value| {
                    to_search_result(
                        profile,
                        "pangolin",
                        MergedRowResult {
                            value,
                            engines: vec![],
                            cached: false,
                        },
                    )
                    .score
                })
                .collect();

            assert!(
                scores.windows(2).all(|w| w[0] >= w[1]),
                "{profile:?} emitted non-descending scores: {scores:?}"
            );
        }
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
            score: 0,
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

    // --- Streaming ------------------------------------------------------

    fn positioned(position: usize, url: &str) -> PositionedRow<CachedResult> {
        PositionedRow {
            position,
            row: MergedRowResult {
                value: r(url),
                engines: vec!["Brave".to_string()],
                cached: false,
            },
        }
    }

    fn done_event(cursor: usize) -> CacheEvent<CachedResult> {
        CacheEvent::Done {
            active_order: 1,
            canonical_order: Some(1),
            next_cursor: cursor,
            has_more: false,
        }
    }

    #[test]
    fn map_cache_event_maps_metadata() {
        let event: CacheEvent<CachedResult> = CacheEvent::Metadata {
            order_id: 42,
            order_kind: OrderKind::Canonical,
            cached: true,
        };
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Meta(meta) => {
                assert_eq!(meta.order_id, 42);
                assert!(meta.canonical);
                assert!(meta.cached);
            }
            other => panic!("expected Meta, got {other:?}"),
        }

        let event: CacheEvent<CachedResult> = CacheEvent::Metadata {
            order_id: 1,
            order_kind: OrderKind::Arrival,
            cached: false,
        };
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Meta(meta) => assert!(!meta.canonical),
            other => panic!("expected Meta, got {other:?}"),
        }
    }

    #[test]
    fn map_cache_event_converts_results_rows_and_preserves_position() {
        let event: CacheEvent<CachedResult> = CacheEvent::Results(vec![
            positioned(3, "https://a.example"),
            positioned(7, "https://b.example"),
        ]);

        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Results(results) => {
                assert_eq!(results.entries.len(), 2);
                assert_eq!(results.entries[0].position, 3);
                assert_eq!(results.entries[0].result.url, "https://a.example");
                assert_eq!(results.entries[1].position, 7);
                assert_eq!(results.entries[1].result.url, "https://b.example");
            }
            other => panic!("expected Results, got {other:?}"),
        }
    }

    #[test]
    fn map_cache_event_maps_attribution() {
        let event: CacheEvent<CachedResult> = CacheEvent::Attribution {
            position: 2,
            url: "https://a.example".into(),
            engines: vec!["Brave".into(), "DuckDuckGo".into()],
        };
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Attribution(a) => {
                assert_eq!(a.position, 2);
                assert_eq!(a.url, "https://a.example");
                assert_eq!(a.engines, vec!["Brave", "DuckDuckGo"]);
            }
            other => panic!("expected Attribution, got {other:?}"),
        }
    }

    #[test]
    fn map_cache_event_maps_engine_outcome_to_status() {
        let event: CacheEvent<CachedResult> = CacheEvent::Engine {
            engine: "Brave".into(),
            outcome: EngineOutcome::TimedOut,
        };
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Engine(report) => {
                assert_eq!(report.engine, "Brave");
                assert!(matches!(report.status, EngineStatus::TimedOut));
            }
            other => panic!("expected Engine, got {other:?}"),
        }
    }

    #[test]
    fn map_cache_event_maps_done() {
        let event: CacheEvent<CachedResult> = CacheEvent::Done {
            active_order: 5,
            canonical_order: Some(9),
            next_cursor: 20,
            has_more: true,
        };
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Done(done) => {
                assert_eq!(done.active_order_id, 5);
                assert_eq!(done.canonical_order_id, Some(9));
                assert_eq!(done.next_cursor, 20);
                assert!(done.has_more);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn map_cache_event_maps_error() {
        let event: CacheEvent<CachedResult> = CacheEvent::Error("db exploded".into());
        match map_cache_event("test", event, convert_for_test) {
            StreamEvent::Error(e) => assert_eq!(e.message, "db exploded"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn map_and_observe_substitutes_error_when_no_usable_engines_and_no_results() {
        let mut acc = StreamAccumulator::default();
        let (mapped, is_terminal) =
            map_and_observe(&mut acc, true, "test", done_event(0), convert_for_test);
        assert!(is_terminal);
        match mapped {
            StreamEvent::Error(e) => {
                assert_eq!(e.message, FetchError::AllEnginesCoolingDown.to_string())
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn map_and_observe_substitutes_error_when_every_contacted_engine_failed() {
        let mut acc = StreamAccumulator::default();
        let engine_event: CacheEvent<CachedResult> = CacheEvent::Engine {
            engine: "Brave".into(),
            outcome: EngineOutcome::Failed("boom".into()),
        };
        map_and_observe(&mut acc, false, "test", engine_event, convert_for_test);

        let (mapped, is_terminal) =
            map_and_observe(&mut acc, false, "test", done_event(0), convert_for_test);
        assert!(is_terminal);
        match mapped {
            StreamEvent::Error(e) => {
                assert_eq!(e.message, FetchError::AllEnginesFailed.to_string())
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn map_and_observe_keeps_done_as_done_when_results_were_seen() {
        let mut acc = StreamAccumulator::default();
        let results: CacheEvent<CachedResult> =
            CacheEvent::Results(vec![positioned(0, "https://a.example")]);
        map_and_observe(&mut acc, false, "test", results, convert_for_test);

        let (mapped, is_terminal) =
            map_and_observe(&mut acc, false, "test", done_event(1), convert_for_test);
        assert!(is_terminal);
        assert!(matches!(mapped, StreamEvent::Done(_)));
    }

    #[test]
    fn map_and_observe_marks_error_event_as_terminal_without_substitution() {
        let mut acc = StreamAccumulator::default();
        let event: CacheEvent<CachedResult> = CacheEvent::Error("db exploded".into());
        let (mapped, is_terminal) =
            map_and_observe(&mut acc, false, "test", event, convert_for_test);
        assert!(is_terminal);
        match mapped {
            StreamEvent::Error(e) => assert_eq!(e.message, "db exploded"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_stream_prepends_id_less_cooling_reports() {
        let subscription = CacheSubscription {
            snapshot: vec![],
            live: mpsc::unbounded_channel().1,
            is_owner: false,
        };
        let cooling = vec![EngineReport {
            engine: "DuckDuckGo".into(),
            status: EngineStatus::CoolingDown("retry in 30s".into()),
        }];
        let mut stream = build_stream(
            "test".into(),
            subscription,
            cooling,
            false,
            convert_for_test,
            |_| {},
        );

        assert_eq!(stream.snapshot.len(), 1);
        assert_eq!(stream.snapshot[0].sequence, None);
        assert!(matches!(stream.snapshot[0].event, StreamEvent::Engine(_)));

        // The upstream sender was dropped before any event was sent, so the
        // spawned forwarding task's `live.recv()` immediately observes a
        // closed channel and returns without emitting anything.
        assert!(stream.live.recv().await.is_none());
    }

    #[tokio::test]
    async fn build_stream_processes_a_terminal_snapshot_synchronously_and_records_once_for_the_owner()
     {
        let snapshot = vec![
            SequencedCacheEvent {
                sequence: 1,
                event: CacheEvent::Metadata {
                    order_id: 1,
                    order_kind: OrderKind::Arrival,
                    cached: false,
                },
            },
            SequencedCacheEvent {
                sequence: 2,
                event: CacheEvent::Results(vec![positioned(0, "https://a.example")]),
            },
            SequencedCacheEvent {
                sequence: 3,
                event: done_event(1),
            },
        ];
        let subscription = CacheSubscription {
            snapshot,
            live: mpsc::unbounded_channel().1,
            is_owner: true,
        };

        let record_calls = Arc::new(StdMutex::new(0));
        let record_calls_clone = record_calls.clone();
        let mut stream = build_stream(
            "test".into(),
            subscription,
            vec![],
            false,
            convert_for_test,
            move |_| {
                *record_calls_clone.lock().unwrap() += 1;
            },
        );

        // Snapshot processing is synchronous: by the time `build_stream`
        // returns, every event -- including the terminal `Done` -- is
        // already in `snapshot`, and `record` has already run. No `.await`
        // was needed to observe either.
        assert_eq!(stream.snapshot.len(), 3);
        assert!(matches!(stream.snapshot[0].event, StreamEvent::Meta(_)));
        assert!(matches!(stream.snapshot[1].event, StreamEvent::Results(_)));
        assert!(matches!(stream.snapshot[2].event, StreamEvent::Done(_)));
        assert_eq!(*record_calls.lock().unwrap(), 1);

        assert!(stream.live.recv().await.is_none());
    }

    #[tokio::test]
    async fn build_stream_does_not_record_for_a_joined_non_owner_subscriber() {
        let snapshot = vec![SequencedCacheEvent {
            sequence: 1,
            event: done_event(0),
        }];
        let subscription = CacheSubscription {
            snapshot,
            live: mpsc::unbounded_channel().1,
            is_owner: false,
        };

        let record_calls = Arc::new(StdMutex::new(0));
        let record_calls_clone = record_calls.clone();
        build_stream(
            "test".into(),
            subscription,
            vec![],
            true,
            convert_for_test,
            move |_| {
                *record_calls_clone.lock().unwrap() += 1;
            },
        );

        assert_eq!(*record_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn build_stream_forwards_live_events_incrementally_without_collecting() {
        // A non-terminal (empty) snapshot means `build_stream` must spawn a
        // forwarding task and return immediately with a live receiver. This
        // proves that receiver yields events one at a time, as they're sent,
        // rather than only after every event has arrived (which would mean
        // the implementation had collected them into a `Vec` first) --
        // exactly the incremental-delivery contract the HTTP layer depends
        // on.
        let (tx, rx) = mpsc::unbounded_channel();
        let subscription = CacheSubscription {
            snapshot: vec![],
            live: rx,
            is_owner: true,
        };

        let record_calls = Arc::new(StdMutex::new(0));
        let record_calls_clone = record_calls.clone();
        let mut stream = build_stream(
            "test".into(),
            subscription,
            vec![],
            false,
            convert_for_test,
            move |_| {
                *record_calls_clone.lock().unwrap() += 1;
            },
        );

        assert!(stream.snapshot.is_empty());

        tx.send(SequencedCacheEvent {
            sequence: 1,
            event: CacheEvent::Results(vec![positioned(0, "https://a.example")]),
        })
        .unwrap();
        let first = stream.live.recv().await.expect("expected the first event");
        assert_eq!(first.sequence, Some(1));
        assert!(matches!(first.event, StreamEvent::Results(_)));

        // Confirm the second event genuinely hasn't arrived yet -- i.e.
        // nothing was pre-collected -- before sending it.
        assert!(
            stream.live.try_recv().is_err(),
            "second event should not be visible before it's sent"
        );

        tx.send(SequencedCacheEvent {
            sequence: 2,
            event: CacheEvent::Results(vec![positioned(1, "https://b.example")]),
        })
        .unwrap();
        let second = stream.live.recv().await.expect("expected the second event");
        assert_eq!(second.sequence, Some(2));

        tx.send(SequencedCacheEvent {
            sequence: 3,
            event: done_event(2),
        })
        .unwrap();
        let third = stream.live.recv().await.expect("expected the done event");
        assert!(matches!(third.event, StreamEvent::Done(_)));

        assert_eq!(*record_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn build_stream_keeps_draining_and_still_records_after_the_downstream_receiver_is_dropped()
     {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscription = CacheSubscription {
            snapshot: vec![],
            live: rx,
            is_owner: true,
        };

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let done_tx = StdMutex::new(Some(done_tx));
        let stream = build_stream(
            "test".into(),
            subscription,
            vec![],
            false,
            convert_for_test,
            move |_| {
                if let Some(done_tx) = done_tx.lock().unwrap().take() {
                    let _ = done_tx.send(());
                }
            },
        );

        // Simulate an HTTP subscriber disconnecting mid-stream: the owner
        // task must keep draining `live` and still record outcomes exactly
        // once, even though nothing downstream is listening anymore.
        drop(stream.live);

        tx.send(SequencedCacheEvent {
            sequence: 1,
            event: CacheEvent::Results(vec![positioned(0, "https://a.example")]),
        })
        .unwrap();
        tx.send(SequencedCacheEvent {
            sequence: 2,
            event: done_event(1),
        })
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), done_rx)
            .await
            .expect("record should still run even though the downstream receiver was dropped")
            .unwrap();
    }
}
