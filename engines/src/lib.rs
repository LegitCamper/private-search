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
    /// Skipped this call, without being contacted, because it's still
    /// cooling down from a recent failure. Never produced by the cache (see
    /// `From<&EngineOutcome>` below) — it's synthesized in the builder for
    /// engines it deliberately didn't pass along as sources.
    CoolingDown(String),
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

fn cached_result_score(result: &CachedResult, query: &str, words: &[String]) -> u32 {
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

    let mut score = url_score(&result.url, words) * 10 + programmer_domain_boost(&result.url);
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

struct DomainWordRanker;

impl Ranker<CachedResult> for DomainWordRanker {
    fn rank(&self, query: &str, mut batch: Vec<CachedResult>) -> Vec<CachedResult> {
        let words = query_words(query);
        batch.sort_by_cached_key(|result| {
            std::cmp::Reverse(cached_result_score(result, query, &words))
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
struct TextSource<E>(E);

#[async_trait]
impl<E> EngineSource<CachedResult> for TextSource<E>
where
    E: SearchEngine + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.0.name()
    }

    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<CachedResult>, String> {
        self.0
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

    async fn fetch_page(&self, query: &str, start: usize) -> Result<Vec<CachedImage>, String> {
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
            .map_err(|e| e.to_string())
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

fn looks_like_stackexchange_query(query: &str) -> bool {
    const HINTS: &[&str] = &[
        "how",
        "why",
        "what",
        "error",
        "exception",
        "panic",
        "failed",
        "failure",
        "cannot",
        "can't",
        "doesn't",
        "compile",
        "compiler",
        "stacktrace",
        "traceback",
        "not working",
    ];

    let query = query.to_lowercase();
    query.contains('?')
        || HINTS.iter().any(|hint| {
            query == *hint
                || query.starts_with(&format!("{hint} "))
                || query.ends_with(&format!(" {hint}"))
                || query.contains(&format!(" {hint} "))
        })
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

    /// Engines used when callers do not choose a set explicitly. GitHub and
    /// the package registries stay enabled for broad queries (so `pangolin`
    /// can find its repository), while Stack Exchange's unusually small
    /// unauthenticated daily quota is reserved for question/error-shaped
    /// searches where it is most likely to add value.
    pub fn defaults_for_query(query: &str) -> Vec<Self> {
        let mut engines = Self::all();
        if !looks_like_stackexchange_query(query) {
            engines.retain(|engine| *engine != Self::StackExchange);
        }
        engines
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
        match self {
            Self::Brave => Arc::new(TextSource(Brave)),
            Self::DuckDuckGo => Arc::new(TextSource(DuckDuckGo)),
            Self::GitHub => Arc::new(TextSource(GitHub)),
            Self::GitLab => Arc::new(TextSource(GitLab)),
            Self::Codeberg => Arc::new(TextSource(Codeberg)),
            Self::StackExchange => Arc::new(TextSource(StackExchange)),
            Self::HackerNews => Arc::new(TextSource(HackerNews)),
            Self::Lobsters => Arc::new(TextSource(Lobsters)),
            Self::Wikipedia => Arc::new(TextSource(Wikipedia)),
            Self::ArchWiki => Arc::new(TextSource(ArchWiki)),
            Self::Mdn => Arc::new(TextSource(Mdn)),
            Self::CratesIo => Arc::new(TextSource(CratesIo)),
            Self::Npm => Arc::new(TextSource(Npm)),
            Self::PyPi => Arc::new(TextSource(PyPi)),
            Self::RubyGems => Arc::new(TextSource(RubyGems)),
            Self::Packagist => Arc::new(TextSource(Packagist)),
            Self::MavenCentral => Arc::new(TextSource(MavenCentral)),
            Self::NuGet => Arc::new(TextSource(NuGet)),
            Self::HexPm => Arc::new(TextSource(HexPm)),
            Self::DockerHub => Arc::new(TextSource(DockerHub)),
            Self::NixPackages => Arc::new(TextSource(NixPackages)),
            Self::GoPkg => Arc::new(TextSource(GoPkg)),
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
            Self::Brave => Arc::new(ImageSource(Brave)),
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
        if let Some((_, outcome)) = outcomes.iter().find(|(n, _)| n == name) {
            match outcome {
                EngineOutcome::Ok => record_success(name),
                EngineOutcome::Failed(_) | EngineOutcome::TimedOut => record_failure(name),
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
fn map_cache_event<R: CacheableRow, T>(
    event: CacheEvent<R>,
    convert: fn(MergedRowResult<R>) -> T,
) -> StreamEvent<T> {
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
                    result: convert(pr.row),
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
fn map_and_observe<R: CacheableRow, T>(
    acc: &mut StreamAccumulator,
    usable_is_empty: bool,
    event: CacheEvent<R>,
    convert: fn(MergedRowResult<R>) -> T,
) -> (StreamEvent<T>, bool) {
    acc.observe(&event);
    let is_done = matches!(event, CacheEvent::Done { .. });
    let is_terminal = is_done || matches!(event, CacheEvent::Error(_));
    let mapped = if is_done {
        match acc.terminal_error_message(usable_is_empty) {
            Some(message) => StreamEvent::Error(StreamErrorPayload { message }),
            None => map_cache_event(event, convert),
        }
    } else {
        map_cache_event(event, convert)
    };
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
fn build_stream<R, T, F>(
    subscription: CacheSubscription<R>,
    cooling_reports: Vec<EngineReport>,
    usable_is_empty: bool,
    convert: fn(MergedRowResult<R>) -> T,
    record: F,
) -> SearchStream<T>
where
    R: CacheableRow,
    T: Send + 'static,
    F: FnOnce(&[(String, EngineOutcome)]) + Send + 'static,
{
    let CacheSubscription {
        snapshot,
        live,
        is_owner,
    } = subscription;

    let mut out_snapshot: Vec<SequencedStreamEvent<T>> = cooling_reports
        .into_iter()
        .map(|report| SequencedStreamEvent {
            sequence: None,
            event: StreamEvent::Engine(report),
        })
        .collect();

    let mut acc = StreamAccumulator::default();
    let mut terminal = false;

    for seq_event in snapshot {
        let SequencedCacheEvent { sequence, event } = seq_event;
        let (mapped, is_terminal) = map_and_observe(&mut acc, usable_is_empty, event, convert);
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
            let (mapped, is_terminal) = map_and_observe(&mut acc, usable_is_empty, event, convert);
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
            engines: Vec::new(),
            start: 0,
            count: DEFAULT_SEARCH_COUNT,
            timeout: default_engine_timeout(),
            order_token: None,
            after_sequence: None,
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
            SearchEngines::defaults_for_query(&self.query)
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

        let extend = text_cache()
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

    /// Streaming equivalent of [`Self::search`]: returns a snapshot of
    /// whatever's already known plus a live channel for events not yet
    /// available, instead of waiting for everything to settle. Reuses the
    /// exact same engine selection, cooldown partitioning, and source
    /// adapters as `.search()` — only the cache call (`subscribe_or_extend`
    /// instead of `get_or_extend`) and the outbound event shape differ.
    pub async fn stream(self) -> Result<SearchStream<SearchResult>, FetchError> {
        let engines = if self.engines.is_empty() {
            SearchEngines::defaults_for_query(&self.query)
        } else {
            self.engines
        };

        let (usable, cooling) = partition_by_cooldown(&engines, SearchEngines::name);
        let sources: Vec<Arc<dyn EngineSource<CachedResult>>> =
            usable.iter().map(|e| e.source()).collect();
        let cooling_reports = cooling_reports(&cooling, SearchEngines::name);

        let subscription = text_cache()
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
            record_outcomes(&record_usable, SearchEngines::name, outcomes);
        };

        Ok(build_stream(
            subscription,
            cooling_reports,
            usable_is_empty,
            to_search_result,
            record,
        ))
    }
}

fn to_search_result(row: MergedRowResult<CachedResult>) -> SearchResult {
    SearchResult {
        url: row.value.url,
        title: row.value.title,
        description: row.value.description,
        engines: row.engines,
        cached: row.cached,
    }
}

fn to_image_result(row: MergedRowResult<CachedImage>) -> ImageResult {
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
            has_more: extend.has_more,
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
    fn broad_defaults_keep_github_but_reserve_stackexchange_quota() {
        let engines = SearchEngines::defaults_for_query("pangolin");
        assert!(engines.contains(&SearchEngines::GitHub));
        assert!(!engines.contains(&SearchEngines::StackExchange));
    }

    #[test]
    fn question_shaped_defaults_include_stackexchange() {
        for query in [
            "why does rust borrow fail?",
            "python traceback",
            "compiler error E0308",
        ] {
            assert!(
                SearchEngines::defaults_for_query(query).contains(&SearchEngines::StackExchange),
                "Stack Exchange should run for {query:?}"
            );
        }
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
    fn built_in_ranker_surfaces_a_matching_github_repository() {
        let ranked = DomainWordRanker.rank(
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
        match map_cache_event(event, to_search_result) {
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
        match map_cache_event(event, to_search_result) {
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

        match map_cache_event(event, to_search_result) {
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
        match map_cache_event(event, to_search_result) {
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
        match map_cache_event(event, to_search_result) {
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
        match map_cache_event(event, to_search_result) {
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
        match map_cache_event(event, to_search_result) {
            StreamEvent::Error(e) => assert_eq!(e.message, "db exploded"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn map_and_observe_substitutes_error_when_no_usable_engines_and_no_results() {
        let mut acc = StreamAccumulator::default();
        let (mapped, is_terminal) =
            map_and_observe(&mut acc, true, done_event(0), to_search_result);
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
        map_and_observe(&mut acc, false, engine_event, to_search_result);

        let (mapped, is_terminal) =
            map_and_observe(&mut acc, false, done_event(0), to_search_result);
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
        map_and_observe(&mut acc, false, results, to_search_result);

        let (mapped, is_terminal) =
            map_and_observe(&mut acc, false, done_event(1), to_search_result);
        assert!(is_terminal);
        assert!(matches!(mapped, StreamEvent::Done(_)));
    }

    #[test]
    fn map_and_observe_marks_error_event_as_terminal_without_substitution() {
        let mut acc = StreamAccumulator::default();
        let event: CacheEvent<CachedResult> = CacheEvent::Error("db exploded".into());
        let (mapped, is_terminal) = map_and_observe(&mut acc, false, event, to_search_result);
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
        let mut stream = build_stream(subscription, cooling, false, to_search_result, |_| {});

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
        let mut stream = build_stream(subscription, vec![], false, to_search_result, move |_| {
            *record_calls_clone.lock().unwrap() += 1;
        });

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
        build_stream(subscription, vec![], true, to_search_result, move |_| {
            *record_calls_clone.lock().unwrap() += 1;
        });

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
        let mut stream = build_stream(subscription, vec![], false, to_search_result, move |_| {
            *record_calls_clone.lock().unwrap() += 1;
        });

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
        let stream = build_stream(subscription, vec![], false, to_search_result, move |_| {
            if let Some(done_tx) = done_tx.lock().unwrap().take() {
                let _ = done_tx.send(());
            }
        });

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
