//! A generic, ranked, append-only merge cache for paginated multi-source
//! search results, backed by SQLite.
//!
//! This crate has no idea what a "search engine" or a "ranking algorithm"
//! is — callers bring their own row type ([`CacheableRow`]), their own
//! ordering policy ([`Ranker`]), and their own way of pulling one more page
//! from a source ([`EngineSource`]). What this crate guarantees in return:
//! results for a query are deduplicated and stored in versioned, persisted
//! orders, so a client's `start`/`count` is always a true index into a
//! *specific* order — no drift between a cold fetch and a cache hit, and no
//! dropped/duplicated pages during pagination, even as newer, better-ranked
//! orders get published behind it.
//!
//! Two kinds of order exist per query: an append-only `arrival` order, built
//! incrementally in engine-completion order while a build is in progress, and
//! immutable `canonical` orders, written once from a full ranking pass over
//! the query's complete membership and published atomically. A client that
//! needs to extend past a canonical order gets handed a forked arrival order
//! (an exact copy of the canonical prefix, then appended to) so its existing
//! pagination is never disturbed.
//!
//! [`MergedCache::subscribe_or_extend`] is the real, incremental API: it
//! returns a [`CacheSubscription`] carrying whatever's already known plus a
//! live channel for events not yet available — a subscriber can render a fast
//! engine's results while a slow one is still in flight. Compatible
//! concurrent requests (same query, same base order, overlapping window)
//! share one owner build instead of duplicating upstream work.
//! [`MergedCache::get_or_extend`] remains as a collecting wrapper over the
//! same subscription, for callers that just want a final, non-streaming
//! result.

mod db;

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::SqlitePool;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, OnceCell, mpsc},
    task::JoinSet,
    time::timeout,
};

pub use db::{OrderKind, init};

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
/// order *within* a batch (for the incremental arrival order) or across the
/// full member set (for a canonical order) — rows already persisted in a
/// given order are never reordered in place.
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
    /// An order token that doesn't exist, or doesn't belong to this query
    /// (including a stale token for a purged query).
    UnknownOrder,
    /// A build failed after producing zero or more prior events (e.g. a DB
    /// error mid-round), surfaced through a stream's [`CacheEvent::Error`].
    Build(String),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Sqlx(e) => write!(f, "cache db error: {e}"),
            CacheError::UnknownOrder => write!(f, "unknown or expired order token"),
            CacheError::Build(msg) => write!(f, "cache build error: {msg}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<sqlx::Error> for CacheError {
    fn from(e: sqlx::Error) -> Self {
        CacheError::Sqlx(e)
    }
}

/// One result row, with the engines that contributed it (accumulated across
/// every round/source that has ever surfaced its URL for this query) and
/// whether it was already cached before this call/stream started.
#[derive(Debug, Clone)]
pub struct MergedRowResult<R> {
    pub value: R,
    pub engines: Vec<String>,
    pub cached: bool,
}

/// A [`MergedRowResult`] at a fixed position within a specific order.
#[derive(Debug, Clone)]
pub struct PositionedRow<R> {
    pub position: usize,
    pub row: MergedRowResult<R>,
}

/// How one source fared on a single call. Timeouts are their own variant
/// (rather than folded into `Failed`) because the timeout itself is this
/// crate's own `round_timeout`, not something the source reported.
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
    /// See [`CacheSubscription::is_owner`].
    pub is_owner: bool,
}

/// One event in a query's result stream. Every event an actual subscriber
/// receives (via [`CacheSubscription`]) has already been filtered/adjusted
/// for that subscriber's requested `[start, start+count)` window.
#[derive(Debug, Clone)]
pub enum CacheEvent<R> {
    /// Which order this stream is building/reading from, and whether it was
    /// (at least initially) served from cache without contacting engines.
    Metadata {
        order_id: i64,
        order_kind: OrderKind,
        cached: bool,
    },
    /// A batch of newly-committed-or-replayed rows, each at its final
    /// position in the active order.
    Results(Vec<PositionedRow<R>>),
    /// `position`'s row gained (or already had) another contributing engine
    /// — carries its full, current engine list as of this event.
    Attribution {
        position: usize,
        url: String,
        engines: Vec<String>,
    },
    /// One source's outcome for this call.
    Engine {
        engine: String,
        outcome: EngineOutcome,
    },
    /// The stream is finished, whether by cache hit or a completed
    /// extension. `active_order` is the order this stream actually read
    /// from/appended to — pass it back as the order token for stable
    /// pagination. `canonical_order` is the query's current best-known
    /// relevance order, if one has ever been published (it may differ from
    /// `active_order`, e.g. when this stream is reading a forked arrival
    /// order). `next_cursor`/`has_more` describe this subscriber's own
    /// window.
    Done {
        active_order: i64,
        canonical_order: Option<i64>,
        next_cursor: usize,
        has_more: bool,
    },
    /// A terminal failure (e.g. a DB error mid-build). No further events
    /// follow for this subscriber.
    Error(String),
}

/// A [`CacheEvent`] tagged with its position in a build's global event
/// history. A live build can resume exactly after `after_sequence`; once its
/// in-memory history is gone, persisted rows may be replayed under newer IDs
/// and consumers deduplicate them by URL/position.
#[derive(Debug, Clone)]
pub struct SequencedCacheEvent<R> {
    pub sequence: u64,
    pub event: CacheEvent<R>,
}

/// The result of [`MergedCache::subscribe_or_extend`]: an already-available
/// snapshot (replayed history, filtered to the caller's window) plus a live
/// channel for whatever hasn't happened yet. Both halves carry the exact same
/// event type, so a caller can just drain `snapshot` then read `live` without
/// caring which events were replay and which were newly produced.
pub struct CacheSubscription<R> {
    pub snapshot: Vec<SequencedCacheEvent<R>>,
    pub live: mpsc::UnboundedReceiver<SequencedCacheEvent<R>>,
    /// True only for the one subscriber whose call actually spawned the
    /// upstream build backing this subscription — a pure cache hit
    /// ([`MergedCache::serve_from_cache`]) and every subscriber that joined
    /// an already-running build are `false`. Meant for a caller that records
    /// engine outcomes exactly once per underlying build even though many
    /// subscribers may share it: only the owner should record, since every
    /// subscriber otherwise observes the same `Engine` events. An owner's own
    /// snapshot (taken strictly before its build is spawned) never contains a
    /// terminal event, so "is_owner" and "terminal event already in this
    /// subscriber's snapshot" can never both be true.
    pub is_owner: bool,
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

/// Adjusts a raw, unfiltered [`CacheEvent`] for one subscriber's requested
/// `[start, start+count)` window: drops `Results`/`Attribution` entries
/// outside it (returning `None` if nothing survives), and recomputes
/// `Done`'s cursor/`has_more` for that window. `Done.next_cursor` on the
/// *input* event is expected to hold the order's full length as of
/// completion (a build-internal convention — see where `Done` is
/// constructed in [`run_build`]), not an already-resolved per-subscriber
/// cursor.
fn transform_for_subscriber<R: Clone>(
    event: &CacheEvent<R>,
    start: usize,
    count: usize,
) -> Option<CacheEvent<R>> {
    let end = start.saturating_add(count);
    match event {
        CacheEvent::Results(rows) => {
            let filtered: Vec<PositionedRow<R>> = rows
                .iter()
                .filter(|r| r.position >= start && r.position < end)
                .cloned()
                .collect();
            (!filtered.is_empty()).then_some(CacheEvent::Results(filtered))
        }
        CacheEvent::Attribution { position, .. } => {
            (*position >= start && *position < end).then(|| event.clone())
        }
        CacheEvent::Done {
            active_order,
            canonical_order,
            next_cursor: order_len,
            has_more,
        } => Some(CacheEvent::Done {
            active_order: *active_order,
            canonical_order: *canonical_order,
            next_cursor: (*order_len).min(end),
            has_more: *has_more || *order_len > end,
        }),
        CacheEvent::Metadata { .. } | CacheEvent::Engine { .. } | CacheEvent::Error(_) => {
            Some(event.clone())
        }
    }
}

/// One subscriber of an in-flight [`Build`]: the window it asked for, and
/// where to send its (transformed) events.
struct Subscriber<R> {
    start: usize,
    count: usize,
    sender: mpsc::UnboundedSender<SequencedCacheEvent<R>>,
}

struct BuildInner<R> {
    /// Every raw event emitted so far, unfiltered, in emission order.
    history: Vec<SequencedCacheEvent<R>>,
    subscribers: Vec<Subscriber<R>>,
    next_sequence: u64,
}

struct BuildControl {
    target_needed_end: usize,
    accepting_subscribers: bool,
}

/// One in-flight (or just-finished-but-not-yet-deregistered) build: the
/// shared state a build's owner task publishes to and every subscriber reads
/// from, guarded by a single async lock so a joining subscriber's
/// snapshot-then-subscribe is race-free against concurrent emission.
struct Build<R> {
    order_id: i64,
    base_order_id: Option<i64>,
    control: Mutex<BuildControl>,
    inner: AsyncMutex<BuildInner<R>>,
    finished: AtomicBool,
    finished_notify: Notify,
}

impl<R: CacheableRow> Build<R> {
    fn new(order_id: i64, base_order_id: Option<i64>, needed_end: usize) -> Arc<Self> {
        Arc::new(Self {
            order_id,
            base_order_id,
            control: Mutex::new(BuildControl {
                target_needed_end: needed_end,
                accepting_subscribers: true,
            }),
            inner: AsyncMutex::new(BuildInner {
                history: Vec::new(),
                subscribers: Vec::new(),
                next_sequence: 0,
            }),
            finished: AtomicBool::new(false),
            finished_notify: Notify::new(),
        })
    }

    fn try_bump_target(&self, needed_end: usize) -> bool {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if !control.accepting_subscribers {
            return false;
        }
        control.target_needed_end = control.target_needed_end.max(needed_end);
        true
    }

    fn finish_if_satisfied(&self, available: usize) -> bool {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if available < control.target_needed_end {
            return false;
        }
        control.accepting_subscribers = false;
        true
    }

    fn stop_accepting_subscribers(&self) {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accepting_subscribers = false;
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
        self.finished_notify.notify_waiters();
    }

    async fn wait_finished(&self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let notified = self.finished_notify.notified();
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// Snapshot + subscribe under one lock: replays history after
    /// `after_sequence` (filtered to `[start, start+count)`), then registers
    /// a live sender, all before releasing the lock — so no event emitted
    /// concurrently with this call can land in the gap between "read
    /// history" and "start listening".
    async fn subscribe(
        self: &Arc<Self>,
        start: usize,
        count: usize,
        after_sequence: Option<u64>,
        is_owner: bool,
    ) -> CacheSubscription<R> {
        let mut inner = self.inner.lock().await;
        let snapshot = inner
            .history
            .iter()
            .filter(|e| after_sequence.is_none_or(|after| e.sequence > after))
            .filter_map(|e| {
                transform_for_subscriber(&e.event, start, count).map(|event| SequencedCacheEvent {
                    sequence: e.sequence,
                    event,
                })
            })
            .collect();
        let (sender, live) = mpsc::unbounded_channel();
        inner.subscribers.push(Subscriber {
            start,
            count,
            sender,
        });
        CacheSubscription {
            snapshot,
            live,
            is_owner,
        }
    }

    /// Appends a raw event to history and fans it out (transformed per
    /// subscriber's window) to every live subscriber, dropping any whose
    /// receiver has been closed.
    async fn emit(&self, event: CacheEvent<R>) {
        let mut inner = self.inner.lock().await;
        let sequence = inner.next_sequence;
        inner.next_sequence += 1;
        inner.subscribers.retain(|sub| {
            match transform_for_subscriber(&event, sub.start, sub.count) {
                Some(transformed) => sub
                    .sender
                    .send(SequencedCacheEvent {
                        sequence,
                        event: transformed,
                    })
                    .is_ok(),
                None => !sub.sender.is_closed(),
            }
        });
        inner.history.push(SequencedCacheEvent { sequence, event });
    }
}

/// Removes `key` from the registry, but only if it still points at `build` —
/// a build that already got replaced (or removed) by someone else is left
/// alone.
async fn remove_build_from_registry<R: CacheableRow>(
    registry: &AsyncMutex<HashMap<i64, Arc<Build<R>>>>,
    query_id: i64,
    build: &Arc<Build<R>>,
) {
    let mut guard = registry.lock().await;
    if let Some(current) = guard.get(&query_id)
        && Arc::ptr_eq(current, build)
    {
        guard.remove(&query_id);
    }
}

/// Ranks the complete current member set for `query_id` and atomically
/// publishes it as a fresh immutable canonical order. Returns the new
/// canonical order id, and whether any of `sources` hasn't yet proven itself
/// exhausted (the source-exhaustion half of `has_more`; the order-length half
/// is computed per-subscriber in [`transform_for_subscriber`]).
async fn finalize_canonical<R: CacheableRow>(
    pool: &SqlitePool,
    ranker: &dyn Ranker<R>,
    query: &str,
    query_id: i64,
    sources: &[Arc<dyn EngineSource<R>>],
) -> Result<(i64, bool), sqlx::Error> {
    let members = db::get_all_members::<R>(pool, query_id).await?;
    let by_url: HashMap<String, i64> = members.iter().map(|m| (m.url.clone(), m.row_id)).collect();
    let ranked = ranker.rank(query, members.into_iter().map(|m| m.value).collect());

    let now = chrono::Utc::now().naive_utc();
    let mut tx = pool.begin().await?;
    let canonical_id = db::create_order(&mut tx, query_id, OrderKind::Canonical, now).await?;
    for (position, value) in ranked.iter().enumerate() {
        // Every ranked row came from `members`, so it must be in `by_url`.
        if let Some(&row_id) = by_url.get(value.url()) {
            db::append_order_entry(&mut tx, canonical_id, row_id, position as i64).await?;
        }
    }
    db::set_canonical_order(&mut tx, query_id, canonical_id).await?;
    tx.commit().await?;

    let mut has_more = false;
    for src in sources {
        let (_, exhausted) = db::get_progress(pool, query_id, src.name()).await?;
        if !exhausted {
            has_more = true;
            break;
        }
    }

    Ok((canonical_id, has_more))
}

/// The owner task for one build: streams engine settlements into `order_id`
/// incrementally (via `join_next`, not a `join_all` barrier) until the
/// requested window is covered or every source is exhausted/benched, then
/// publishes a fresh canonical order over the complete member set. Runs
/// detached from any particular subscriber/caller — it owns only cloned,
/// `'static` state, so it keeps going even if every subscriber (including
/// the original caller) disconnects, and it outlives the `MergedCache<R>`
/// instance's own borrow scope (important since tests construct one as a
/// plain local variable, not a `'static` global).
struct RunBuildParams<R: CacheableRow> {
    pool: SqlitePool,
    ranker: Arc<dyn Ranker<R>>,
    registry: Arc<AsyncMutex<HashMap<i64, Arc<Build<R>>>>>,
    registry_key: i64,
    build: Arc<Build<R>>,
    query: String,
    query_id: i64,
    sources: Vec<Arc<dyn EngineSource<R>>>,
    round_timeout: Duration,
}

async fn run_build<R: CacheableRow>(params: RunBuildParams<R>) {
    let RunBuildParams {
        pool,
        ranker,
        registry,
        registry_key,
        build,
        query,
        query_id,
        sources,
        round_timeout,
    } = params;
    let order_id = build.order_id;

    build
        .emit(CacheEvent::Metadata {
            order_id,
            order_kind: OrderKind::Arrival,
            cached: false,
        })
        .await;

    let base_len = match db::order_len(&pool, order_id).await {
        Ok(len) => len,
        Err(e) => {
            build.emit(CacheEvent::Error(e.to_string())).await;
            remove_build_from_registry(&registry, registry_key, &build).await;
            return;
        }
    };

    // Seed dedup/position state from whatever this order already has (a
    // forked canonical prefix, or leftovers from an earlier call against
    // this same arrival order), and replay it as an immediate "here's what's
    // cached so far" burst — this is also what makes the legacy collector
    // correct for a partially-covered window, not just a fresh one.
    let mut known_urls: HashMap<String, i64> = HashMap::new();
    let mut position_of: HashMap<i64, i64> = HashMap::new();

    if base_len > 0 {
        match db::get_order_window::<R>(&pool, order_id, query_id, 0, base_len as usize).await {
            Ok(prefix) => {
                let mut positioned = Vec::with_capacity(prefix.len());
                for entry in prefix {
                    known_urls.insert(entry.row.url.clone(), entry.row.row_id);
                    position_of.insert(entry.row.row_id, entry.position);
                    positioned.push(PositionedRow {
                        position: entry.position as usize,
                        row: MergedRowResult {
                            value: entry.row.value,
                            engines: entry.row.engines,
                            cached: true,
                        },
                    });
                }
                if !positioned.is_empty() {
                    build.emit(CacheEvent::Results(positioned)).await;
                }
            }
            Err(e) => {
                build.emit(CacheEvent::Error(e.to_string())).await;
                remove_build_from_registry(&registry, registry_key, &build).await;
                return;
            }
        }
    }

    let mut next_start: HashMap<&'static str, i64> = HashMap::new();
    let mut exhausted: HashMap<&'static str, bool> = HashMap::new();
    for src in &sources {
        match db::get_progress(&pool, query_id, src.name()).await {
            Ok((ns, ex)) => {
                next_start.insert(src.name(), ns);
                exhausted.insert(src.name(), ex);
            }
            Err(e) => {
                build.emit(CacheEvent::Error(e.to_string())).await;
                remove_build_from_registry(&registry, registry_key, &build).await;
                return;
            }
        }
    }

    // Sources that failed/timed out once this call are benched for the
    // remaining rounds: hammering a struggling engine up to MAX_ROUNDS times
    // back-to-back is exactly the traffic pattern that gets an IP
    // bot-walled. They retry fresh on the next call/build instead.
    let mut failed_this_call: HashSet<&'static str> = HashSet::new();
    let mut position = base_len;

    for _round in 0..MAX_ROUNDS {
        if build.finish_if_satisfied(position as usize) {
            break;
        }

        let needy: Vec<Arc<dyn EngineSource<R>>> = sources
            .iter()
            .filter(|s| !exhausted[s.name()] && !failed_this_call.contains(s.name()))
            .cloned()
            .collect();
        if needy.is_empty() {
            build.stop_accepting_subscribers();
            break;
        }

        // Frozen at round start: judging "did this engine's page contain
        // anything new" against this snapshot (rather than the live,
        // continuously-updated `known_urls`) means two sources that
        // legitimately surface the same fresh URL in the same round don't
        // falsely mark each other exhausted.
        let round_start_urls: HashSet<String> = known_urls.keys().cloned().collect();

        let mut set = JoinSet::new();
        for src in needy {
            let q = query.clone();
            let start_for_src = next_start[src.name()] as usize;
            set.spawn(async move {
                let outcome = timeout(round_timeout, src.fetch_page(&q, start_for_src)).await;
                (src.name(), outcome)
            });
        }

        // `join_next` (not `join_all`/collecting into a `Vec` first) is the
        // whole point: each engine's settlement is committed and emitted the
        // instant it lands, so a fast source's rows reach subscribers while
        // a slower one in this same round is still pending.
        while let Some(joined) = set.join_next().await {
            let (name, outcome) = match joined {
                Ok(v) => v,
                Err(join_err) => {
                    log::warn!("engine task join error: {join_err}");
                    continue;
                }
            };

            match outcome {
                Ok(Ok(rows)) => {
                    let raw_count = rows.len();
                    let mut new_count = 0usize;
                    let mut fresh: Vec<db::FreshRow<R>> = Vec::new();
                    let mut rediscovered: Vec<(i64, String)> = Vec::new();
                    let mut seen_this_engine: HashSet<String> = HashSet::new();

                    for r in rows {
                        let url = r.url().to_string();
                        if !seen_this_engine.insert(url.clone()) {
                            continue;
                        }
                        if !round_start_urls.contains(&url) {
                            new_count += 1;
                        }
                        if let Some(&row_id) = known_urls.get(&url) {
                            rediscovered.push((row_id, url));
                        } else {
                            fresh.push(db::FreshRow { url, value: r });
                        }
                    }

                    exhausted.insert(name, raw_count == 0 || new_count == 0);
                    next_start.insert(name, next_start[name] + raw_count as i64);

                    // The ranker only sees this engine's genuinely novel rows
                    // for this round; it drops the URL association, but every
                    // `CacheableRow` reproduces its own via `.url()`.
                    let ranked_fresh: Vec<db::FreshRow<R>> = ranker
                        .rank(&query, fresh.into_iter().map(|f| f.value).collect())
                        .into_iter()
                        .map(|value| db::FreshRow {
                            url: value.url().to_string(),
                            value,
                        })
                        .collect();

                    let now = chrono::Utc::now().naive_utc();
                    let commit = db::commit_engine_round(
                        &pool,
                        db::EngineRoundCommit {
                            query_id,
                            order_id,
                            start_position: position,
                            engine_name: name,
                            next_start: next_start[name],
                            exhausted: exhausted[name],
                            fetched_at: now,
                            fresh: ranked_fresh,
                            rediscovered,
                        },
                    )
                    .await;

                    let commit = match commit {
                        Ok(c) => c,
                        Err(e) => {
                            build.emit(CacheEvent::Error(e.to_string())).await;
                            remove_build_from_registry(&registry, registry_key, &build).await;
                            return;
                        }
                    };

                    if !commit.fresh.is_empty() {
                        let mut positioned = Vec::with_capacity(commit.fresh.len());
                        for (row_id, pos, item) in commit.fresh {
                            known_urls.insert(item.url.clone(), row_id);
                            position_of.insert(row_id, pos);
                            positioned.push(PositionedRow {
                                position: pos as usize,
                                row: MergedRowResult {
                                    value: item.value,
                                    engines: vec![name.to_string()],
                                    cached: false,
                                },
                            });
                        }
                        position += positioned.len() as i64;
                        build.emit(CacheEvent::Results(positioned)).await;
                    }

                    for (row_id, url, engines) in commit.rediscovered_engines {
                        if let Some(&pos) = position_of.get(&row_id) {
                            build
                                .emit(CacheEvent::Attribution {
                                    position: pos as usize,
                                    url,
                                    engines,
                                })
                                .await;
                        }
                    }

                    build
                        .emit(CacheEvent::Engine {
                            engine: name.to_string(),
                            outcome: EngineOutcome::Ok,
                        })
                        .await;
                }
                Ok(Err(e)) => {
                    log::warn!("source \"{name}\" failed: {e}");
                    failed_this_call.insert(name);
                    build
                        .emit(CacheEvent::Engine {
                            engine: name.to_string(),
                            outcome: EngineOutcome::Failed(e),
                        })
                        .await;
                }
                Err(_) => {
                    log::warn!("source \"{name}\" timed out");
                    failed_this_call.insert(name);
                    build
                        .emit(CacheEvent::Engine {
                            engine: name.to_string(),
                            outcome: EngineOutcome::TimedOut,
                        })
                        .await;
                }
            }
        }
    }

    // No subscriber may join once finalization starts. `try_bump_target` and
    // the satisfied check share one mutex, so a wider late request is either
    // observed by another round or waits for a follow-up build.
    build.stop_accepting_subscribers();

    match finalize_canonical(&pool, ranker.as_ref(), &query, query_id, &sources).await {
        Ok((canonical_id, source_has_more)) => {
            let order_len = db::order_len(&pool, order_id).await.unwrap_or(position);
            // `next_cursor` here is a build-internal convention: the order's
            // full length, not yet resolved to any one subscriber's cursor.
            // `transform_for_subscriber` does that resolution on delivery.
            build
                .emit(CacheEvent::Done {
                    active_order: order_id,
                    canonical_order: Some(canonical_id),
                    next_cursor: order_len as usize,
                    has_more: source_has_more,
                })
                .await;
        }
        Err(e) => {
            build.emit(CacheEvent::Error(e.to_string())).await;
        }
    }

    remove_build_from_registry(&registry, registry_key, &build).await;
}

pub struct MergedCache<R: CacheableRow> {
    pool: SqlitePool,
    namespace: &'static str,
    ranker: Arc<dyn Ranker<R>>,
    /// At most one extending build per query. Requests against the same base
    /// order join it; requests against a different generation wait for it to
    /// finish and then re-evaluate persisted progress before starting. This
    /// serializes the query-scoped engine cursor and canonical publication
    /// without blocking unrelated queries.
    builds: Arc<AsyncMutex<HashMap<i64, Arc<Build<R>>>>>,
}

impl<R: CacheableRow> MergedCache<R> {
    pub fn new(pool: SqlitePool, namespace: &'static str, ranker: Arc<dyn Ranker<R>>) -> Self {
        Self {
            pool,
            namespace,
            ranker,
            builds: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Serves `[start, start+count)` for `query` entirely from an
    /// already-covered order, without contacting any source. Used by
    /// [`Self::subscribe_or_extend`] once it's determined the base order
    /// already covers the requested window.
    #[allow(clippy::too_many_arguments)]
    async fn serve_from_cache(
        &self,
        query_id: i64,
        order_id: i64,
        order_kind: OrderKind,
        start: usize,
        count: usize,
        sources: &[Arc<dyn EngineSource<R>>],
        after_sequence: Option<u64>,
    ) -> Result<CacheSubscription<R>, CacheError> {
        let rows = db::get_order_window::<R>(&self.pool, order_id, query_id, start, count).await?;
        let order_len = db::order_len(&self.pool, order_id).await? as usize;
        let canonical_order_id = db::get_canonical_order_id(&self.pool, query_id).await?;

        let end = start.saturating_add(count);
        let mut has_more = order_len > end;
        if !has_more {
            for src in sources {
                let (_, exhausted) = db::get_progress(&self.pool, query_id, src.name()).await?;
                if !exhausted {
                    has_more = true;
                    break;
                }
            }
        }

        let positioned: Vec<PositionedRow<R>> = rows
            .into_iter()
            .map(|r| PositionedRow {
                position: r.position as usize,
                row: MergedRowResult {
                    value: r.row.value,
                    engines: r.row.engines,
                    cached: true,
                },
            })
            .collect();

        // A reconnect can arrive after its in-memory build has already
        // completed and been deregistered. The persisted cache cannot recover
        // that build's exact event history, so replay the covered rows with
        // IDs strictly above the caller's cursor; URL/position deduplication in
        // the consumer makes the replay visible exactly once.
        let first_sequence = after_sequence.map_or(0, |after| after.saturating_add(1));
        let mut snapshot = vec![SequencedCacheEvent {
            sequence: first_sequence,
            event: CacheEvent::Metadata {
                order_id,
                order_kind,
                cached: true,
            },
        }];
        if !positioned.is_empty() {
            snapshot.push(SequencedCacheEvent {
                sequence: first_sequence.saturating_add(1),
                event: CacheEvent::Results(positioned),
            });
        }
        snapshot.push(SequencedCacheEvent {
            sequence: first_sequence.saturating_add(snapshot.len() as u64),
            event: CacheEvent::Done {
                active_order: order_id,
                canonical_order: canonical_order_id,
                next_cursor: order_len.min(end),
                has_more,
            },
        });

        // No build backs a pure cache hit, so the live half of the
        // subscription is simply already-closed.
        let (_tx, live) = mpsc::unbounded_channel();
        Ok(CacheSubscription {
            snapshot,
            live,
            is_owner: false,
        })
    }

    /// The real, incremental API: returns rows `[start, start+count)` for
    /// `query` as a [`CacheSubscription`] — a snapshot of whatever's already
    /// known plus a live channel for the rest.
    ///
    /// If `order_token` is given, pagination continues that exact order
    /// (resolved against `query`, so a token from a different query/purged
    /// query yields [`CacheError::UnknownOrder`]). Otherwise the query's
    /// current canonical order is used if one exists.
    ///
    /// If the resolved order already covers the window, sources are never
    /// contacted. Otherwise: an immutable canonical order is forked into a
    /// fresh appendable arrival order (preserving the exact existing
    /// prefix), or a brand-new empty arrival order is created for a fresh
    /// query; `sources` are then queried incrementally (each settling
    /// independently, not behind a round barrier) until the window is
    /// covered or every source is exhausted/benched, at which point a new
    /// canonical order is ranked and published. A compatible concurrent
    /// request (same query, same base order) joins that same build rather
    /// than starting a second one, and the build itself keeps running to
    /// completion even if every subscriber disconnects.
    ///
    /// `after_sequence` resumes a still-live build after a given sequence. If
    /// that build already finished, the persisted window is replayed with new
    /// sequence IDs above the supplied cursor; consumers must deduplicate rows
    /// by URL/position, as the browser client does.
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe_or_extend(
        &self,
        query: &str,
        sources: Vec<Arc<dyn EngineSource<R>>>,
        start: usize,
        count: usize,
        order_token: Option<i64>,
        round_timeout: Duration,
        after_sequence: Option<u64>,
    ) -> Result<CacheSubscription<R>, CacheError> {
        let needed_end = start.saturating_add(count);
        let now = chrono::Utc::now().naive_utc();

        let mut tx = self.pool.begin().await?;
        let namespace_id = db::get_or_create_namespace(&mut tx, self.namespace).await?;
        let query_id = db::get_or_create_query(&mut tx, query, namespace_id, now).await?;
        tx.commit().await?;

        loop {
            let (base_order_id, base_kind) = match order_token {
                Some(id) => {
                    let kind = db::get_order_kind(&self.pool, id, query_id)
                        .await?
                        .ok_or(CacheError::UnknownOrder)?;
                    (Some(id), Some(kind))
                }
                None => {
                    let canonical = db::get_canonical_order_id(&self.pool, query_id).await?;
                    (canonical, canonical.map(|_| OrderKind::Canonical))
                }
            };

            let base_len = match base_order_id {
                Some(id) => db::order_len(&self.pool, id).await? as usize,
                None => 0,
            };

            if let Some(order_id) = base_order_id
                && base_len >= needed_end
            {
                return self
                    .serve_from_cache(
                        query_id,
                        order_id,
                        base_kind.unwrap(),
                        start,
                        count,
                        &sources,
                        after_sequence,
                    )
                    .await;
            }

            let mut builds = self.builds.lock().await;
            if let Some(build) = builds.get(&query_id).cloned() {
                if build.base_order_id == base_order_id && build.try_bump_target(needed_end) {
                    drop(builds);
                    return Ok(build.subscribe(start, count, after_sequence, false).await);
                }

                // A different order generation, or a build already entering
                // finalization, cannot safely share query-scoped engine
                // progress. Wait for it to deregister, then resolve the latest
                // canonical/progress state again.
                drop(builds);
                build.wait_finished().await;
                continue;
            }

            let target_order_id = match base_order_id {
                Some(id) => {
                    match base_kind.expect("base_kind is always resolved alongside base_order_id") {
                        OrderKind::Arrival => id,
                        OrderKind::Canonical => {
                            let mut tx = self.pool.begin().await?;
                            let forked = db::fork_order(&mut tx, query_id, id, now).await?;
                            tx.commit().await?;
                            forked
                        }
                    }
                }
                None => {
                    let mut tx = self.pool.begin().await?;
                    let created =
                        db::create_order(&mut tx, query_id, OrderKind::Arrival, now).await?;
                    tx.commit().await?;
                    created
                }
            };

            let build = Build::<R>::new(target_order_id, base_order_id, needed_end);
            builds.insert(query_id, build.clone());
            // Subscribe before releasing the registry lock and before spawning
            // the owner, so its first event cannot beat registration.
            let subscription = build.subscribe(start, count, after_sequence, true).await;
            drop(builds);

            let cleanup_registry = self.builds.clone();
            let cleanup_build = build.clone();
            let params = RunBuildParams {
                pool: self.pool.clone(),
                ranker: self.ranker.clone(),
                registry: self.builds.clone(),
                registry_key: query_id,
                build,
                query: query.to_string(),
                query_id,
                sources,
                round_timeout,
            };
            tokio::spawn(async move {
                let result = tokio::spawn(run_build(params)).await;
                if let Err(join_err) = result {
                    cleanup_build.stop_accepting_subscribers();
                    cleanup_build
                        .emit(CacheEvent::Error(format!(
                            "search build failed: {join_err}"
                        )))
                        .await;
                }
                remove_build_from_registry(&cleanup_registry, query_id, &cleanup_build).await;
                cleanup_build.mark_finished();
            });

            return Ok(subscription);
        }
    }

    /// Returns rows `[start, start+count)` for `query`, extending the merged
    /// cache from `sources` until the window is satisfied or every source is
    /// exhausted. A collecting wrapper over [`Self::subscribe_or_extend`],
    /// kept for existing non-streaming callers/tests; it does not duplicate
    /// any orchestration logic of its own.
    pub async fn get_or_extend(
        &self,
        query: &str,
        sources: &[Arc<dyn EngineSource<R>>],
        start: usize,
        count: usize,
        round_timeout: Duration,
    ) -> Result<ExtendResult<R>, CacheError> {
        let subscription = self
            .subscribe_or_extend(
                query,
                sources.to_vec(),
                start,
                count,
                None,
                round_timeout,
                None,
            )
            .await?;

        let mut rows_by_position: std::collections::BTreeMap<usize, MergedRowResult<R>> =
            std::collections::BTreeMap::new();
        let mut engine_outcomes = Vec::new();
        let mut has_more = false;

        let CacheSubscription {
            snapshot,
            mut live,
            is_owner,
        } = subscription;
        let mut events = snapshot.into_iter();

        loop {
            let next = match events.next() {
                Some(e) => Some(e),
                None => live.recv().await,
            };
            let Some(seq_event) = next else { break };
            match seq_event.event {
                CacheEvent::Metadata { .. } => {}
                CacheEvent::Results(rows) => {
                    for pr in rows {
                        rows_by_position.insert(pr.position, pr.row);
                    }
                }
                CacheEvent::Attribution {
                    position, engines, ..
                } => {
                    if let Some(existing) = rows_by_position.get_mut(&position) {
                        existing.engines = engines;
                    }
                }
                CacheEvent::Engine { engine, outcome } => {
                    engine_outcomes.push((engine, outcome));
                }
                CacheEvent::Done { has_more: hm, .. } => {
                    has_more = hm;
                }
                CacheEvent::Error(message) => {
                    return Err(CacheError::Build(message));
                }
            }
        }

        let rows = rows_by_position
            .into_iter()
            .filter(|(pos, _)| *pos >= start && *pos < start + count)
            .map(|(_, row)| row)
            .collect();

        Ok(ExtendResult {
            rows,
            has_more,
            engine_outcomes,
            is_owner,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{
        collections::VecDeque,
        str::FromStr,
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
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

    /// Reverses each batch — used where a test needs the canonical order to
    /// visibly differ from arrival order.
    struct ReverseRanker;
    impl Ranker<TestRow> for ReverseRanker {
        fn rank(&self, _query: &str, mut batch: Vec<TestRow>) -> Vec<TestRow> {
            batch.reverse();
            batch
        }
    }

    struct PanicOnceRanker(AtomicBool);
    impl Ranker<TestRow> for PanicOnceRanker {
        fn rank(&self, _query: &str, batch: Vec<TestRow>) -> Vec<TestRow> {
            if self.0.swap(false, Ordering::SeqCst) {
                panic!("intentional ranker panic");
            }
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

    /// A source whose page is fixed regardless of `start`: every call
    /// returns the identical rows. Models Brave's image endpoint, whose
    /// static page can't be paginated and re-serves its first batch on
    /// every request.
    struct RepeatingSource {
        name: &'static str,
        page: Vec<TestRow>,
        calls: Arc<AtomicUsize>,
    }

    impl RepeatingSource {
        fn new(name: &'static str, page: Vec<TestRow>) -> Arc<Self> {
            Arc::new(Self {
                name,
                page,
                calls: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait]
    impl EngineSource<TestRow> for RepeatingSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn fetch_page(&self, _query: &str, _start: usize) -> Result<Vec<TestRow>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.page.clone())
        }
    }

    /// A source gated on a [`tokio::sync::Notify`]: `fetch_page` blocks until
    /// the test explicitly releases it, letting tests deterministically
    /// observe "this engine hasn't settled yet" without a race on timing.
    struct GatedSource {
        name: &'static str,
        gate: Arc<tokio::sync::Notify>,
        page: Vec<TestRow>,
        calls: Arc<AtomicUsize>,
    }

    impl GatedSource {
        fn new(
            name: &'static str,
            gate: Arc<tokio::sync::Notify>,
            page: Vec<TestRow>,
        ) -> Arc<Self> {
            Arc::new(Self {
                name,
                gate,
                page,
                calls: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait]
    impl EngineSource<TestRow> for GatedSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn fetch_page(&self, _query: &str, _start: usize) -> Result<Vec<TestRow>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
            Ok(self.page.clone())
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

    async fn test_cache_with_ranker(ranker: Arc<dyn Ranker<TestRow>>) -> MergedCache<TestRow> {
        let options = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        db::create_schema(&pool).await.unwrap();
        MergedCache::new(pool, "test", ranker)
    }

    fn sources<S: EngineSource<TestRow> + 'static>(
        v: Vec<Arc<S>>,
    ) -> Vec<Arc<dyn EngineSource<TestRow>>> {
        v.into_iter()
            .map(|s| s as Arc<dyn EngineSource<TestRow>>)
            .collect()
    }

    fn one_source<S: EngineSource<TestRow> + 'static>(
        s: Arc<S>,
    ) -> Vec<Arc<dyn EngineSource<TestRow>>> {
        sources(vec![s])
    }

    /// Drains a subscription (snapshot then live) until `Done`, returning
    /// every event seen along the way.
    async fn drain_all(sub: CacheSubscription<TestRow>) -> Vec<SequencedCacheEvent<TestRow>> {
        let mut out = sub.snapshot;
        let mut live = sub.live;
        loop {
            match out.last() {
                Some(SequencedCacheEvent {
                    event: CacheEvent::Done { .. } | CacheEvent::Error(_),
                    ..
                }) => break,
                _ => match live.recv().await {
                    Some(e) => out.push(e),
                    None => break,
                },
            }
        }
        out
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
        let source =
            ScriptedSource::new("A", vec![(0..12).map(|i| row(&format!("u{i}"))).collect()]);
        let calls = source.calls.clone();

        let page1 = cache
            .get_or_extend(
                "q",
                &one_source(source.clone()),
                0,
                5,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(page1.rows.len(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Simulate a totally separate later request (e.g. a different
        // client/process) for page 2 — everything it needs is already in the
        // merged cache from page 1's over-fetch.
        let page2 = cache
            .get_or_extend(
                "q",
                &one_source(source.clone()),
                5,
                5,
                Duration::from_secs(1),
            )
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
    async fn a_source_that_only_repeats_known_rows_is_marked_exhausted() {
        let cache = test_cache().await;
        let page = vec![row("a"), row("b"), row("c"), row("d"), row("e")];
        let source = RepeatingSource::new("Repeater", page);
        let calls = source.calls.clone();

        cache
            .get_or_extend(
                "q",
                &one_source(source.clone()),
                0,
                5,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        // Simulate the frontend's "scroll for more" pattern: a later request
        // asks for the next page.
        let result = cache
            .get_or_extend("q", &one_source(source), 5, 5, Duration::from_secs(1))
            .await
            .unwrap();

        assert!(
            !result.has_more,
            "a source that only re-serves rows already known must be treated as exhausted"
        );
        assert!(
            calls.load(Ordering::SeqCst) <= 2,
            "an exhausted source must not keep being re-contacted"
        );
    }

    #[tokio::test]
    async fn a_source_is_not_exhausted_just_because_another_source_found_the_same_url() {
        let cache = test_cache().await;
        let a = ScriptedSource::new("A", vec![vec![row("x"), row("y")]]);
        let b = ScriptedSource::new("B", vec![vec![row("x"), row("y")]]);

        let result = cache
            .get_or_extend("q", &sources(vec![a, b]), 0, 2, Duration::from_secs(1))
            .await
            .unwrap();

        assert!(
            result.has_more,
            "overlapping fresh URLs within one round must not count as proof of exhaustion"
        );
    }

    #[tokio::test]
    async fn has_more_reflects_the_source_that_is_still_alive() {
        let cache = test_cache().await;
        let short = ScriptedSource::new("Short", vec![vec![row("s1")], vec![]]);
        let long = ScriptedSource::new(
            "Long",
            vec![
                vec![row("l1"), row("l2")],
                vec![row("l3"), row("l4")],
                vec![],
            ],
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

        assert!(
            result.has_more,
            "Long hasn't proven exhaustion within the rounds needed to fill the window"
        );
    }

    #[tokio::test]
    async fn duplicate_rows_from_one_source_do_not_duplicate_engine_attribution() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("shared"), row("shared")]]);

        let result = cache
            .get_or_extend("q", &one_source(source), 0, 1, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].engines, vec!["A".to_string()]);
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
        assert_eq!(
            result2.rows.len(),
            0,
            "no new merged row — it's the same URL"
        );

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
        assert_eq!(
            engines3,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    #[tokio::test]
    async fn purge_stale_removes_old_queries_and_orphaned_rows() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("old")]]);
        cache
            .get_or_extend(
                "stale query",
                &one_source(source),
                0,
                1,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        // Backdate it directly, then purge with a cutoff that only catches it.
        let (query_id,): (i64,) =
            sqlx::query_as("SELECT id FROM queries WHERE query = 'stale query'")
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
    /// potentially lock-contending, DB writes) — the second should join the
    /// first's in-flight build and reuse what it produces.
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

    // --- New streaming-specific tests -------------------------------------

    /// The core streaming guarantee: a fast engine's committed row must reach
    /// a live subscriber while a slower, gated engine in the same round is
    /// still pending — proving results aren't collected into a `Vec` and
    /// returned only once everything settles.
    #[tokio::test]
    async fn fast_result_is_receivable_before_a_gated_slow_source_completes() {
        let cache = test_cache().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let fast = ScriptedSource::new("Fast", vec![vec![row("f1")]]);
        let slow = GatedSource::new("Slow", gate.clone(), vec![row("s1")]);

        let mut sub = cache
            .subscribe_or_extend(
                "streaming",
                sources(vec![fast])
                    .into_iter()
                    .chain(one_source(slow))
                    .collect(),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        let mut saw_fast_result = sub
            .snapshot
            .iter()
            .any(|e| matches!(e.event, CacheEvent::Results(_)));

        if !saw_fast_result {
            let event = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = sub
                        .live
                        .recv()
                        .await
                        .expect("stream ended before any result");
                    if matches!(event.event, CacheEvent::Results(_)) {
                        return event;
                    }
                }
            })
            .await
            .expect("timed out waiting for the fast engine's result");
            assert!(matches!(event.event, CacheEvent::Results(_)));
            saw_fast_result = true;
        }
        assert!(
            saw_fast_result,
            "fast engine's result must be visible before the slow engine is released"
        );

        // Now let the slow engine finish and drain to completion.
        gate.notify_one();
        let mut saw_done = false;
        while let Some(event) = sub.live.recv().await {
            if matches!(event.event, CacheEvent::Done { .. }) {
                saw_done = true;
                break;
            }
        }
        assert!(
            saw_done,
            "stream must still reach Done after the gate opens"
        );
    }

    /// A second identical in-flight request must not trigger a second
    /// upstream call, and must still see a race-free snapshot + live replay
    /// (no missed or duplicated rows) even though it joined mid-build.
    #[tokio::test]
    async fn second_identical_subscriber_joins_the_same_build_without_a_second_upstream_call() {
        let cache = test_cache().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let source = GatedSource::new("Gated", gate.clone(), vec![row("a"), row("b")]);
        let calls = source.calls.clone();

        let sub1 = cache
            .subscribe_or_extend(
                "shared",
                one_source(source),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        // The engine is now gated mid-fetch; a second identical request must
        // join the same build rather than firing its own fetch.
        let sub2 = cache
            .subscribe_or_extend(
                "shared",
                Vec::<Arc<dyn EngineSource<TestRow>>>::new(),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        gate.notify_one();

        let events1 = drain_all(sub1).await;
        let events2 = drain_all(sub2).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "upstream must be called exactly once"
        );

        let urls_of = |events: &[SequencedCacheEvent<TestRow>]| -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match &e.event {
                    CacheEvent::Results(rows) => Some(
                        rows.iter()
                            .map(|r| r.row.value.url.clone())
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .flatten()
                .collect()
        };
        let mut urls1 = urls_of(&events1);
        let mut urls2 = urls_of(&events2);
        urls1.sort();
        urls2.sort();
        assert_eq!(urls1, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(urls2, vec!["a".to_string(), "b".to_string()]);
        assert!(
            events1
                .iter()
                .any(|e| matches!(e.event, CacheEvent::Done { .. }))
        );
        assert!(
            events2
                .iter()
                .any(|e| matches!(e.event, CacheEvent::Done { .. }))
        );
    }

    /// A URL discovered by one engine and rediscovered by another in the
    /// same build must produce exactly one `Results` entry, plus a later
    /// `Attribution` event reflecting both engines.
    #[tokio::test]
    async fn cross_engine_duplicate_url_emits_one_result_and_an_attribution_update() {
        let cache = test_cache().await;
        let a = ScriptedSource::new("A", vec![vec![row("shared")]]);
        let b = ScriptedSource::new("B", vec![vec![row("shared")]]);

        let sub = cache
            .subscribe_or_extend(
                "dup",
                sources(vec![a, b]),
                0,
                1,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        let events = drain_all(sub).await;

        let result_count = events
            .iter()
            .filter(|e| matches!(e.event, CacheEvent::Results(_)))
            .flat_map(|e| match &e.event {
                CacheEvent::Results(rows) => rows.clone(),
                _ => unreachable!(),
            })
            .count();
        assert_eq!(
            result_count, 1,
            "must emit exactly one Results entry for the shared URL"
        );

        let last_attribution = events.iter().rev().find_map(|e| match &e.event {
            CacheEvent::Attribution { engines, .. } => Some(engines.clone()),
            _ => None,
        });
        let mut engines = last_attribution.expect("expected an Attribution event");
        engines.sort();
        assert_eq!(engines, vec!["A".to_string(), "B".to_string()]);
    }

    /// The first (arrival) order a client sees must not be reshuffled by a
    /// later, differently-ranked canonical order — a fresh untokened request
    /// afterward gets the canonical (ranked) order instead.
    #[tokio::test]
    async fn arrival_order_is_stable_while_a_later_canonical_order_can_differ() {
        let cache = test_cache_with_ranker(Arc::new(ReverseRanker)).await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b"), row("c")]]);

        let sub = cache
            .subscribe_or_extend(
                "order",
                one_source(source),
                0,
                3,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        let events = drain_all(sub).await;

        let arrival_urls: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.event {
                CacheEvent::Results(rows) => Some(
                    rows.iter()
                        .map(|r| r.row.value.url.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        // A build locally ranks each engine's fresh batch before persisting
        // it into the arrival order (a single round here), so the arrival
        // order is the source order run through `ReverseRanker` once.
        assert_eq!(arrival_urls, vec!["c", "b", "a"]);

        // A fresh untokened request now sees the *canonical* order: a full
        // ranking pass over the complete member set (ordered by row
        // insertion id, i.e. arrival order) run through `ReverseRanker`
        // again — landing back at the original source order, which is
        // exactly why it must differ from the arrival order above.
        let result = cache
            .get_or_extend(
                "order",
                &one_source(ScriptedSource::new("A2", vec![])),
                0,
                3,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let canonical_urls: Vec<String> = result.rows.iter().map(|r| r.value.url.clone()).collect();
        assert_eq!(canonical_urls, vec!["a", "b", "c"]);
        assert_ne!(
            canonical_urls, arrival_urls,
            "canonical order must actually differ from arrival order for this test to mean anything"
        );
    }

    /// Pagination against a specific (tokened) order must remain stable even
    /// after a later, untokened extension publishes a different canonical
    /// generation.
    #[tokio::test]
    async fn tokened_pagination_is_stable_after_a_later_canonical_republish() {
        let cache = test_cache_with_ranker(Arc::new(ReverseRanker)).await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b")]]);

        let sub = cache
            .subscribe_or_extend(
                "stable",
                one_source(source),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        let events = drain_all(sub).await;
        let (order_token, first_urls): (i64, Vec<String>) = {
            let mut token = None;
            let mut urls = Vec::new();
            for e in &events {
                match &e.event {
                    CacheEvent::Results(rows) => {
                        urls.extend(rows.iter().map(|r| r.row.value.url.clone()));
                    }
                    CacheEvent::Done { active_order, .. } => token = Some(*active_order),
                    _ => {}
                }
            }
            (token.unwrap(), urls)
        };

        // A later untokened extension adds a new row and republishes a new
        // (reversed) canonical order.
        let extend_source = ScriptedSource::new("B", vec![vec![row("c")]]);
        cache
            .get_or_extend(
                "stable",
                &one_source(extend_source),
                2,
                1,
                Duration::from_secs(5),
            )
            .await
            .unwrap();

        // Re-request the ORIGINAL token/window: must still see the original
        // two rows, unaffected by the new canonical generation.
        let resumed = cache
            .subscribe_or_extend(
                "stable",
                Vec::new(),
                0,
                2,
                Some(order_token),
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        let resumed_events = drain_all(resumed).await;
        let resumed_urls: Vec<String> = resumed_events
            .iter()
            .filter_map(|e| match &e.event {
                CacheEvent::Results(rows) => Some(
                    rows.iter()
                        .map(|r| r.row.value.url.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(resumed_urls, first_urls);
    }

    /// `after_sequence` replay against a still-in-flight build must deliver
    /// every remaining event exactly once — no gaps, no duplicates.
    #[tokio::test]
    async fn after_sequence_replay_has_no_gaps_or_duplicates() {
        let cache = test_cache().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let fast = ScriptedSource::new("Fast", vec![vec![row("f1")]]);
        let slow = GatedSource::new("Slow", gate.clone(), vec![row("s1")]);

        let sub1 = cache
            .subscribe_or_extend(
                "resume",
                sources(vec![fast])
                    .into_iter()
                    .chain(one_source(slow))
                    .collect(),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();

        // Wait for at least one event beyond Metadata so we have a real
        // sequence number to resume after.
        let mut sub1 = sub1;
        let mut seen_seqs: Vec<u64> = sub1.snapshot.iter().map(|e| e.sequence).collect();
        while seen_seqs.len() < 2 {
            let event = sub1.live.recv().await.unwrap();
            seen_seqs.push(event.sequence);
        }
        let resume_after = *seen_seqs.iter().max().unwrap();

        // A second subscriber joins mid-build, asking only for what's new.
        let sub2 = cache
            .subscribe_or_extend(
                "resume",
                Vec::new(),
                0,
                2,
                None,
                Duration::from_secs(5),
                Some(resume_after),
            )
            .await
            .unwrap();

        gate.notify_one();

        let events2 = drain_all(sub2).await;
        let seqs2: Vec<u64> = events2.iter().map(|e| e.sequence).collect();
        assert!(
            seqs2.iter().all(|s| *s > resume_after),
            "resumed subscriber must never see a sequence at or before its resume point"
        );
        let mut sorted = seqs2.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seqs2.len(),
            "resumed subscriber must not see duplicate sequence numbers"
        );
        assert!(
            seqs2.windows(2).all(|w| w[0] < w[1]),
            "resumed subscriber's sequence numbers must be strictly increasing"
        );

        // Drain the first subscriber to completion too, and check its own
        // sequence stream has no gaps from 0 up to (and including) Done.
        while let Some(event) = sub1.live.recv().await {
            seen_seqs.push(event.sequence);
            if matches!(event.event, CacheEvent::Done { .. }) {
                break;
            }
        }
        let mut all_seqs = seen_seqs.clone();
        all_seqs.sort();
        all_seqs.dedup();
        assert_eq!(
            all_seqs,
            seen_seqs
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }

    /// Once a window is fully covered by the canonical order, a repeat
    /// request must not contact sources — but must still correctly report
    /// `has_more` when a source hasn't proven exhaustion.
    #[tokio::test]
    async fn cached_covered_window_skips_sources_but_still_reports_has_more() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b")]]);
        let calls = source.calls.clone();

        let first = cache
            .get_or_extend(
                "covered",
                &one_source(source.clone()),
                0,
                2,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(first.rows.len(), 2);
        assert!(
            first.has_more,
            "source returned a full page and hasn't proven exhaustion"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Same exact window again — fully covered by the canonical order
        // published after the first call.
        let second = cache
            .get_or_extend("covered", &one_source(source), 0, 2, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a fully-covered window must not recontact sources"
        );
        assert_eq!(second.rows.len(), 2);
        assert!(second.rows.iter().all(|r| r.cached));
        assert!(
            second.has_more,
            "has_more must still reflect that the source hasn't proven exhaustion, \
             even though the window itself is fully covered by the cache"
        );
    }

    /// A source that never returns within `round_timeout` must surface as
    /// `EngineOutcome::TimedOut`, not hang or panic the build.
    #[tokio::test]
    async fn slow_source_beyond_round_timeout_reports_timed_out() {
        let cache = test_cache().await;
        let gate = Arc::new(tokio::sync::Notify::new()); // never notified
        let never = GatedSource::new("Never", gate, vec![row("x")]);

        let result = cache
            .get_or_extend(
                "timeout",
                &one_source(never),
                0,
                1,
                Duration::from_millis(50),
            )
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 0);
        assert!(matches!(
            result.engine_outcomes.as_slice(),
            [(name, EngineOutcome::TimedOut)] if name == "Never"
        ));
    }

    /// A source returning `Err` must surface as `EngineOutcome::Failed`
    /// rather than aborting the whole build.
    #[tokio::test]
    async fn failing_source_reports_failed_outcome() {
        struct FailingSource;
        #[async_trait]
        impl EngineSource<TestRow> for FailingSource {
            fn name(&self) -> &'static str {
                "Failing"
            }
            async fn fetch_page(
                &self,
                _query: &str,
                _start: usize,
            ) -> Result<Vec<TestRow>, String> {
                Err("upstream exploded".to_string())
            }
        }

        let cache = test_cache().await;
        let result = cache
            .get_or_extend(
                "failure",
                &one_source(Arc::new(FailingSource)),
                0,
                1,
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 0);
        assert!(matches!(
            result.engine_outcomes.as_slice(),
            [(name, EngineOutcome::Failed(msg))] if name == "Failing" && msg == "upstream exploded"
        ));
    }

    // --- `is_owner` tests ---------------------------------------------------

    /// The subscriber whose call actually spawns a new build must be the
    /// owner; a second, concurrent subscriber that joins the same in-flight
    /// build must not be.
    #[tokio::test]
    async fn only_the_build_spawning_subscriber_is_the_owner() {
        let cache = test_cache().await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let source = GatedSource::new("Gated", gate.clone(), vec![row("a")]);

        let sub1 = cache
            .subscribe_or_extend(
                "owner-test",
                one_source(source),
                0,
                1,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        assert!(
            sub1.is_owner,
            "the subscriber that spawned the build must own it"
        );

        let sub2 = cache
            .subscribe_or_extend(
                "owner-test",
                Vec::<Arc<dyn EngineSource<TestRow>>>::new(),
                0,
                1,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        assert!(
            !sub2.is_owner,
            "a subscriber that joins an already-running build must not own it"
        );

        gate.notify_one();
        drain_all(sub1).await;
        drain_all(sub2).await;
    }

    /// A pure cache hit (`serve_from_cache`) is never owned — there's no
    /// build backing it at all.
    #[tokio::test]
    async fn cache_hit_subscription_is_never_the_owner() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b")]]);

        let first = cache
            .get_or_extend(
                "cached",
                &one_source(source.clone()),
                0,
                2,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(
            first.is_owner,
            "the call that actually built the cache owns it"
        );

        let sub = cache
            .subscribe_or_extend(
                "cached",
                one_source(source),
                0,
                2,
                None,
                Duration::from_secs(5),
                None,
            )
            .await
            .unwrap();
        assert!(
            !sub.is_owner,
            "a fully cache-served request has no build to own"
        );
    }

    /// Regression test for the underlying bug `is_owner` exists to prevent:
    /// two concurrent identical `get_or_extend` calls must only have the
    /// owner's outcomes attributable, i.e. exactly one of the two joined
    /// results reports `is_owner == true`. (Callers that gate cooldown
    /// recording on `is_owner` therefore record each build's outcomes
    /// exactly once, even though both callers observe the same `Engine`
    /// events.)
    #[tokio::test]
    async fn exactly_one_of_two_concurrent_identical_extends_is_the_owner() {
        let cache = test_cache().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let source: Arc<dyn EngineSource<TestRow>> = Arc::new(SlowCountingSource {
            calls: calls.clone(),
        });
        let src_vec = vec![source];

        let (a, b) = tokio::join!(
            cache.get_or_extend("owner race", &src_vec, 0, 5, Duration::from_secs(1)),
            cache.get_or_extend("owner race", &src_vec, 0, 5, Duration::from_secs(1)),
        );
        let a = a.unwrap();
        let b = b.unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "still only one upstream call for two concurrent identical requests"
        );
        assert_eq!(
            [a.is_owner, b.is_owner].iter().filter(|&&o| o).count(),
            1,
            "exactly one of two joined concurrent requests must be the owner"
        );
    }

    #[tokio::test]
    async fn completed_build_reconnect_uses_sequences_above_the_resume_cursor() {
        let cache = test_cache().await;
        let source = ScriptedSource::new("A", vec![vec![row("a"), row("b")]]);

        let first = cache
            .subscribe_or_extend(
                "completed reconnect",
                one_source(source.clone()),
                0,
                2,
                None,
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        let first_events = drain_all(first).await;
        let order_token = first_events
            .iter()
            .find_map(|event| match event.event {
                CacheEvent::Done { active_order, .. } => Some(active_order),
                _ => None,
            })
            .unwrap();

        let resumed = cache
            .subscribe_or_extend(
                "completed reconnect",
                one_source(source),
                0,
                2,
                Some(order_token),
                Duration::from_secs(1),
                Some(100),
            )
            .await
            .unwrap();

        assert!(resumed.snapshot.iter().all(|event| event.sequence > 100));
        assert!(
            resumed
                .snapshot
                .iter()
                .any(|event| matches!(event.event, CacheEvent::Results(_)))
        );
        assert!(matches!(
            resumed.snapshot.last().map(|event| &event.event),
            Some(CacheEvent::Done { .. })
        ));
    }

    #[tokio::test]
    async fn different_order_builds_for_one_query_are_serialized() {
        let cache = Arc::new(test_cache().await);
        let initial_source = ScriptedSource::new("A", vec![vec![row("a")]]);
        let initial = cache
            .subscribe_or_extend(
                "serialized generations",
                one_source(initial_source),
                0,
                1,
                None,
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        let initial_events = drain_all(initial).await;
        let old_arrival_order = initial_events
            .iter()
            .find_map(|event| match event.event {
                CacheEvent::Done { active_order, .. } => Some(active_order),
                _ => None,
            })
            .unwrap();

        let gate = Arc::new(Notify::new());
        let gated = GatedSource::new("A", gate.clone(), vec![row("b")]);
        let calls = gated.calls.clone();
        let old_order_build = cache
            .subscribe_or_extend(
                "serialized generations",
                one_source(gated.clone()),
                0,
                2,
                Some(old_arrival_order),
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();

        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }

        let other_cache = cache.clone();
        let newer_request = tokio::spawn(async move {
            other_cache
                .subscribe_or_extend(
                    "serialized generations",
                    one_source(gated),
                    0,
                    2,
                    None,
                    Duration::from_secs(1),
                    None,
                )
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!newer_request.is_finished());

        gate.notify_one();
        drain_all(old_order_build).await;
        let newer = tokio::time::timeout(Duration::from_secs(1), newer_request)
            .await
            .expect("newer order request stayed blocked")
            .unwrap();
        let newer_events = drain_all(newer).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            newer_events
                .iter()
                .any(|event| matches!(event.event, CacheEvent::Metadata { cached: true, .. }))
        );
    }

    #[tokio::test]
    async fn panicking_build_emits_error_and_deregisters() {
        let cache = MergedCache::new(
            test_cache().await.pool,
            "panic-test",
            Arc::new(PanicOnceRanker(AtomicBool::new(true))),
        );
        let first_source = ScriptedSource::new("A", vec![vec![row("a")]]);
        let first = cache
            .subscribe_or_extend(
                "panic cleanup",
                one_source(first_source),
                0,
                1,
                None,
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        let first_events = tokio::time::timeout(Duration::from_secs(1), drain_all(first))
            .await
            .expect("panicking build left its subscription open");
        assert!(
            first_events
                .iter()
                .any(|event| matches!(event.event, CacheEvent::Error(_)))
        );

        let second_source = ScriptedSource::new("A", vec![vec![row("b")]]);
        let second = cache
            .subscribe_or_extend(
                "panic cleanup",
                one_source(second_source),
                0,
                1,
                None,
                Duration::from_secs(1),
                None,
            )
            .await
            .unwrap();
        let second_events = tokio::time::timeout(Duration::from_secs(1), drain_all(second))
            .await
            .expect("query stayed wedged after build panic");
        assert!(
            second_events
                .iter()
                .any(|event| matches!(event.event, CacheEvent::Done { .. }))
        );
    }

    #[test]
    fn finalization_closes_the_late_target_race() {
        let build = Build::<TestRow>::new(1, None, 2);
        assert!(build.try_bump_target(3));
        assert!(!build.finish_if_satisfied(2));
        assert!(build.finish_if_satisfied(3));
        assert!(!build.try_bump_target(4));
    }
}
