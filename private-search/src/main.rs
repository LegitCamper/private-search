use std::pin::Pin;
use std::time::Duration;

use rocket::{
    Build, Orbit, Request, Response, Rocket,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    futures::stream::{self, Stream},
    http::Status,
    response::{
        Redirect,
        stream::{Event, EventStream},
    },
    serde::{Deserialize, Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

use private_search_engines::{
    FetchError, ImageEngines, ImageResult, ImageSearchBuilder, SearchBuilder, SearchResponse,
    SearchResult, SearchStream, SequencedStreamEvent, StreamEvent, init_db,
};

mod rate_limit;
use rate_limit::{RateLimited, RateLimiter};

#[macro_use]
extern crate rocket;

/// Rejected outright rather than handed to the engines — keeps `start + count`
/// from ever overflowing and caps how deep a client can push pagination in
/// one request.
const MAX_START: usize = 10_000;
const MAX_COUNT: usize = 25;

/// Cache-busts static assets referenced from templates (`?v={{version}}`):
/// `CacheFairing` sets a 24h `max-age` on `/static/*`, so without this a
/// deploy that changes `search.js`/`styles.css` could leave stale copies in
/// clients' caches for up to a day.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolves an asset dir from `env_var` if set, otherwise falls back to a path
/// relative to this crate (baked in at compile time via `CARGO_MANIFEST_DIR`).
/// The fallback makes `cargo run` work from anywhere (repo root or this crate's
/// dir); the env var lets deployments (e.g. Docker) point at wherever the
/// assets actually land at runtime, since the compile-time path won't exist
/// outside the machine/container that built the binary.
fn resolve_dir(env_var: &str, manifest_relative_default: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| manifest_relative_default.to_string())
}

/// Reads `env_var` as a `u64` seconds count, falling back to `default_secs`
/// if unset or unparseable.
fn resolve_secs(env_var: &str, default_secs: u64) -> Duration {
    std::env::var(env_var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

fn build_rocket() -> Rocket<Build> {
    let static_dir = resolve_dir("STATIC_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/static"));
    let template_dir = resolve_dir(
        "TEMPLATE_DIR",
        concat!(env!("CARGO_MANIFEST_DIR"), "/templates"),
    );

    let cache_clean_interval = resolve_secs("CACHE_CLEAN_INTERVAL_SECS", 60 * 60); // hourly
    let cache_max_age = resolve_secs("CACHE_MAX_AGE_SECS", 24 * 60 * 60); // 1 day

    let figment = rocket::Config::figment().merge(("template_dir", template_dir));

    rocket::custom(figment)
        .attach(Template::fairing())
        .attach(CacheFairing)
        .attach(CacheCleanupFairing {
            interval: cache_clean_interval,
            max_age: cache_max_age,
        })
        .manage(RateLimiter::default())
        .mount("/static", FileServer::from(static_dir))
        .mount(
            "/",
            routes![index, empty_search, search, query, query_stream, health],
        )
}

#[rocket::main]
#[expect(clippy::result_large_err, reason = "signature is fixed by rocket")]
async fn main() -> Result<(), rocket::Error> {
    init_db().await;

    build_rocket().ignite().await?.launch().await?;

    Ok(())
}

/// Periodically purges stale cache entries (see
/// [`private_search_engines::clean_cache`]) so the SQLite cache doesn't grow
/// forever. Runs as an `on_liftoff` fairing rather than being spawned
/// straight from `main` so it starts only once Rocket (and its logger) is
/// actually up.
struct CacheCleanupFairing {
    interval: Duration,
    max_age: Duration,
}

#[rocket::async_trait]
impl Fairing for CacheCleanupFairing {
    fn info(&self) -> Info {
        Info {
            name: "Cache cleanup scheduler",
            kind: Kind::Liftoff,
        }
    }

    async fn on_liftoff(&self, _rocket: &Rocket<Orbit>) {
        let interval = self.interval;
        let max_age = self.max_age;

        rocket::tokio::spawn(async move {
            let mut ticker = rocket::tokio::time::interval(interval);
            // `interval` fires immediately on its first tick; that's fine —
            // it just means cleanup also runs once right at startup.
            loop {
                ticker.tick().await;
                match private_search_engines::clean_cache(max_age).await {
                    Ok(purged) if purged > 0 => {
                        log::info!(
                            "cache cleanup: purged {purged} stale quer{}",
                            if purged == 1 { "y" } else { "ies" }
                        );
                    }
                    Ok(_) => {}
                    Err(e) => log::error!("cache cleanup failed: {e}"),
                }
            }
        });
    }
}

pub struct CacheFairing;

#[rocket::async_trait]
impl Fairing for CacheFairing {
    fn info(&self) -> Info {
        Info {
            name: "Add cache headers to files",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, req: &'r Request<'_>, res: &mut Response<'r>) {
        let path = req.uri().path();

        if path.starts_with("/static/") {
            // Safe to cache hard: templates request these with `?v={version}`,
            // so a deploy that changes an asset changes its URL.
            res.set_header(rocket::http::Header::new(
                "Cache-Control",
                "public, max-age=86400",
            ));
        } else if path.starts_with("/query/stream") {
            // Every frame is either freshly-fetched or a live cache replay
            // keyed by an opaque order token/cursor the client already holds
            // -- an intermediary caching (or replaying) an SSE body would be
            // actively wrong, not just wasteful.
            res.set_header(rocket::http::Header::new("Cache-Control", "no-store"));
        } else if path.starts_with("/search") || path == "/" {
            // The pages themselves must revalidate. They carry the `?v=`
            // markers that bust every other asset, so caching them for a day
            // pins a returning visitor to a day-old page — including its old
            // asset URLs, which is how a shipped frontend fix can go
            // completely unnoticed by the people who use the site most.
            res.set_header(rocket::http::Header::new("Cache-Control", "no-cache"));
        }
    }
}

#[get("/")]
fn index() -> Template {
    Template::render(
        "index",
        context! {
            title: "Homepage",
            version: VERSION,
        },
    )
}

#[get("/search")]
fn empty_search() -> Redirect {
    Redirect::to("/")
}

/// Liveness/readiness probe target for container orchestration. Doesn't
/// touch the DB or upstream engines — if the process can respond at all,
/// it's up; readiness w.r.t. the cache DB is already covered by `init_db()`
/// blocking startup in `main`.
#[get("/health")]
fn health() -> Status {
    Status::Ok
}

#[allow(unused_variables)]
#[get("/search?<t>&<q>")]
fn search(t: Option<String>, q: &str) -> Template {
    Template::render(
        "search",
        context! {
            title: "Search",
            version: VERSION,
        },
    )
}

#[derive(Serialize, Debug)]
#[serde(crate = "rocket::serde")]
pub enum QueryResults {
    General(SearchResponse<SearchResult>),
    Images(SearchResponse<ImageResult>),
}

/// Everything `/query` returns on failure is JSON too — no bare-string
/// bodies — so clients never need to sniff response text to tell an error
/// from a result.
#[derive(Serialize, Deserialize, Debug)]
#[serde(crate = "rocket::serde")]
pub struct ApiErrorBody {
    error: String,
}

fn api_error(status: Status, message: impl Into<String>) -> (Status, Json<ApiErrorBody>) {
    (
        status,
        Json(ApiErrorBody {
            error: message.into(),
        }),
    )
}

/// Shared `FetchError` -> (status, logged message, JSON body) mapping for
/// both `/query` and `/query/stream`'s pre-stream setup errors. Once
/// `/query/stream` has actually started emitting frames, a `FetchError` can
/// no longer reach this helper — see `event_for`/`is_terminal` instead, which
/// surface an `error` SSE event.
fn map_fetch_error(e: FetchError, tab: &str, query: &str) -> (Status, Json<ApiErrorBody>) {
    let status = if e.is_unknown_order() {
        Status::BadRequest
    } else {
        match &e {
            FetchError::Cache(_) => Status::InternalServerError,
            FetchError::AllEnginesFailed => Status::BadGateway,
            FetchError::AllEnginesCoolingDown => Status::ServiceUnavailable,
        }
    };
    match &e {
        FetchError::Cache(cache_err) if e.is_unknown_order() => {
            log::warn!("unknown or expired order token: tab={tab} query={query:?}: {cache_err}")
        }
        FetchError::Cache(cache_err) => log::error!("cache db error: {cache_err}"),
        FetchError::AllEnginesFailed => {
            log::error!("all engines failed: tab={tab} query={query:?}")
        }
        // Not an error worth alarming on: the cooldown is working as
        // intended, deliberately skipping engines that recently failed
        // instead of hammering them again.
        FetchError::AllEnginesCoolingDown => {
            log::warn!("all engines cooling down: tab={tab} query={query:?}")
        }
    }
    let message = match &e {
        FetchError::Cache(_) if e.is_unknown_order() => "unknown or expired order token",
        FetchError::Cache(_) => "search cache failed",
        FetchError::AllEnginesFailed => "all search engines failed or timed out",
        FetchError::AllEnginesCoolingDown => {
            "all search engines are temporarily paused after recent failures"
        }
    };
    api_error(status, message)
}

#[get("/query?<tab>&<query>&<start>&<count>")]
async fn query(
    _limit: RateLimited,
    tab: &str,
    query: &str,
    start: usize,
    count: usize,
) -> Result<Json<QueryResults>, (Status, Json<ApiErrorBody>)> {
    if count == 0 || count > MAX_COUNT {
        return Err(api_error(
            Status::BadRequest,
            format!("count must be between 1 and {MAX_COUNT}"),
        ));
    }
    if start > MAX_START {
        return Err(api_error(
            Status::BadRequest,
            format!("start must not exceed {MAX_START}"),
        ));
    }

    let results = match tab {
        "General" | "general" => SearchBuilder::new(query)
            .start(start)
            .count(count)
            .search()
            .await
            .map(QueryResults::General),
        "Images" | "images" => ImageSearchBuilder::new(query)
            .engine(ImageEngines::Brave)
            .start(start)
            .count(count)
            .search()
            .await
            .map(QueryResults::Images),
        _ => return Err(api_error(Status::BadRequest, "unknown tab requested")),
    }
    .map_err(|e| map_fetch_error(e, tab, query))?;

    log::debug!(
        "query ok: tab={tab} query={query:?} results={}",
        results_len(&results)
    );

    Ok(Json(results))
}

fn results_len(results: &QueryResults) -> usize {
    match results {
        QueryResults::General(r) => r.results.len(),
        QueryResults::Images(r) => r.results.len(),
    }
}

/// True once `event`'s `StreamEvent` is a terminal frame (`done` or
/// `error`) — the point after which `event_stream_from` must stop yielding,
/// mirroring `/query`'s single JSON response ending the request.
fn is_terminal<T>(event: &StreamEvent<T>) -> bool {
    matches!(event, StreamEvent::Done(_) | StreamEvent::Error(_))
}

/// Maps one engine-layer [`SequencedStreamEvent`] onto the wire SSE `Event`:
/// JSON body via `Event::json`, the `event:` name from
/// [`StreamEvent::name`], and (only for cache-derived, sequenced frames) an
/// `id:` for `after_sequence` reconnect.
fn event_for<T: Serialize>(seq_event: SequencedStreamEvent<T>) -> Event {
    let event = Event::json(&seq_event.event).event(seq_event.event.name());
    match seq_event.sequence {
        Some(sequence) => event.id(sequence.to_string()),
        None => event,
    }
}

/// Turns a [`SearchStream`] into the SSE response body: the snapshot is
/// drained synchronously first (it's already available), then the live
/// receiver is awaited and forwarded event-by-event as they arrive — at no
/// point is the remaining live stream collected into a `Vec` before being
/// returned to the client, so a client genuinely receives cache rows as soon
/// as an engine (or a concurrent joined build) commits them. Stops right
/// after yielding a terminal (`done`/`error`) frame, same as `/query`
/// returning its one JSON response.
///
/// Both tabs (`SearchResult`/`ImageResult`) go through this one function, so
/// the two `/query/stream` branches must return the exact same concrete
/// response type — the boxed trait object is what makes that possible
/// despite `T` differing between them.
fn event_stream_from<T>(
    search_stream: SearchStream<T>,
) -> EventStream<Pin<Box<dyn Stream<Item = Event> + Send>>>
where
    T: Serialize + Send + 'static,
{
    struct State<T> {
        snapshot: std::vec::IntoIter<SequencedStreamEvent<T>>,
        live: rocket::tokio::sync::mpsc::UnboundedReceiver<SequencedStreamEvent<T>>,
        done: bool,
    }

    let state = State {
        snapshot: search_stream.snapshot.into_iter(),
        live: search_stream.live,
        done: false,
    };

    let boxed: Pin<Box<dyn Stream<Item = Event> + Send>> =
        Box::pin(stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }

            let seq_event = match state.snapshot.next() {
                Some(seq_event) => seq_event,
                None => state.live.recv().await?,
            };

            state.done = is_terminal(&seq_event.event);
            let event = event_for(seq_event);
            Some((event, state))
        }));

    EventStream::from(boxed).heartbeat(Duration::from_secs(30))
}

#[get("/query/stream?<tab>&<query>&<start>&<count>&<order>&<after>")]
async fn query_stream(
    _limit: RateLimited,
    tab: &str,
    query: &str,
    start: usize,
    count: usize,
    order: Option<i64>,
    after: Option<u64>,
) -> Result<EventStream<Pin<Box<dyn Stream<Item = Event> + Send>>>, (Status, Json<ApiErrorBody>)> {
    if count == 0 || count > MAX_COUNT {
        return Err(api_error(
            Status::BadRequest,
            format!("count must be between 1 and {MAX_COUNT}"),
        ));
    }
    if start > MAX_START {
        return Err(api_error(
            Status::BadRequest,
            format!("start must not exceed {MAX_START}"),
        ));
    }

    match tab {
        "General" | "general" => {
            let stream = SearchBuilder::new(query)
                .start(start)
                .count(count)
                .order_token(order)
                .after_sequence(after)
                .stream()
                .await
                .map_err(|e| map_fetch_error(e, tab, query))?;
            Ok(event_stream_from(stream))
        }
        "Images" | "images" => {
            let stream = ImageSearchBuilder::new(query)
                .engine(ImageEngines::Brave)
                .start(start)
                .count(count)
                .order_token(order)
                .after_sequence(after)
                .stream()
                .await
                .map_err(|e| map_fetch_error(e, tab, query))?;
            Ok(event_stream_from(stream))
        }
        _ => Err(api_error(Status::BadRequest, "unknown tab requested")),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rocket::local::asynchronous::Client;

    async fn client() -> Client {
        Client::tracked(build_rocket())
            .await
            .expect("failed to build test rocket instance")
    }

    #[rocket::async_test]
    async fn health_returns_ok() {
        let client = client().await;
        let res = client.get("/health").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
    }

    #[rocket::async_test]
    async fn index_renders() {
        let client = client().await;
        let res = client.get("/").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
    }

    #[rocket::async_test]
    async fn search_page_renders() {
        let client = client().await;
        let res = client.get("/search?q=rust&t=general").dispatch().await;
        assert_eq!(res.status(), Status::Ok);
    }

    #[rocket::async_test]
    async fn empty_search_redirects_home() {
        let client = client().await;
        let res = client.get("/search").dispatch().await;
        assert_eq!(res.status(), Status::SeeOther);
        assert_eq!(res.headers().get_one("Location"), Some("/"));
    }

    #[rocket::async_test]
    async fn search_pages_are_revalidated_but_static_assets_are_cached() {
        let client = client().await;

        let page = client.get("/search?q=rust&t=general").dispatch().await;
        assert_eq!(
            page.headers().get_one("Cache-Control"),
            Some("no-cache"),
            "a cached search page keeps serving stale `?v=` asset URLs"
        );

        let asset = client.get("/static/search-core.js").dispatch().await;
        assert_eq!(
            asset.headers().get_one("Cache-Control"),
            Some("public, max-age=86400")
        );
    }

    #[rocket::async_test]
    async fn query_rejects_count_over_max() {
        let client = client().await;
        let res = client
            .get("/query?tab=general&query=rust&start=0&count=999")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("count"));
    }

    #[rocket::async_test]
    async fn query_rejects_count_of_zero() {
        let client = client().await;
        let res = client
            .get("/query?tab=general&query=rust&start=0&count=0")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
    }

    #[rocket::async_test]
    async fn query_rejects_start_over_max() {
        let client = client().await;
        let res = client
            .get("/query?tab=general&query=rust&start=999999999&count=10")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("start"));
    }

    #[rocket::async_test]
    async fn query_rejects_unknown_tab() {
        let client = client().await;
        let res = client
            .get("/query?tab=bogus&query=rust&start=0&count=10")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("tab"));
    }

    #[rocket::async_test]
    async fn query_enforces_rate_limit() {
        let client = client().await;
        let mut saw_429 = false;
        // MAX_REQUESTS_PER_WINDOW is 30; a bogus tab short-circuits before
        // any network call, so this stays fast and hits the limiter directly.
        for _ in 0..40 {
            let res = client
                .get("/query?tab=bogus&query=rust&start=0&count=1")
                .dispatch()
                .await;
            if res.status() == Status::TooManyRequests {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "expected to eventually be rate limited");
    }

    // --- `/query/stream` -----------------------------------------------
    //
    // Validation/rate-limit/header tests hit the real route with a bogus
    // tab, which (like `/query`) short-circuits before any network or cache
    // call, so these stay fast and deterministic. Content-type/SSE-shape and
    // live-incremental-delivery tests instead go through a dedicated
    // test-only route (`test_stream_route`) that calls `event_stream_from`
    // directly on a hand-built `SearchStream`, so they never touch a real
    // engine or the process-global cache either.

    #[rocket::async_test]
    async fn query_stream_rejects_count_over_max() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=general&query=rust&start=0&count=999")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("count"));
    }

    #[rocket::async_test]
    async fn query_stream_rejects_count_of_zero() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=general&query=rust&start=0&count=0")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
    }

    #[rocket::async_test]
    async fn query_stream_rejects_start_over_max() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=general&query=rust&start=999999999&count=10")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("start"));
    }

    #[rocket::async_test]
    async fn query_stream_rejects_unknown_tab() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=bogus&query=rust&start=0&count=10")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert!(body.error.contains("tab"));
    }

    #[rocket::async_test]
    async fn query_stream_reports_an_unknown_order_as_a_client_error() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=general&query=unknown-order-regression&start=0&count=1&order=9223372036854775807")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        let body: ApiErrorBody = res.into_json().await.expect("expected a JSON error body");
        assert_eq!(body.error, "unknown or expired order token");
    }

    #[rocket::async_test]
    async fn query_stream_validation_errors_stay_json_not_sse() {
        let client = client().await;
        let res = client
            .get("/query/stream?tab=bogus&query=rust&start=0&count=10")
            .dispatch()
            .await;
        assert_eq!(res.status(), Status::BadRequest);
        assert_eq!(
            res.headers().get_one("Content-Type"),
            Some("application/json")
        );
    }

    #[rocket::async_test]
    async fn query_stream_sets_cache_control_no_store() {
        let client = client().await;
        // A bogus tab short-circuits validation before any engine/cache
        // call, but `CacheFairing` runs on every response regardless of
        // status, so this alone is enough to pin the header.
        let res = client
            .get("/query/stream?tab=bogus&query=rust&start=0&count=10")
            .dispatch()
            .await;
        assert_eq!(res.headers().get_one("Cache-Control"), Some("no-store"));
    }

    #[rocket::async_test]
    async fn query_stream_enforces_rate_limit() {
        let client = client().await;
        let mut saw_429 = false;
        for _ in 0..40 {
            let res = client
                .get("/query/stream?tab=bogus&query=rust&start=0&count=1")
                .dispatch()
                .await;
            if res.status() == Status::TooManyRequests {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "expected to eventually be rate limited");
    }

    // --- SSE shape + live-incremental delivery, via a test-only route ---

    /// Holds the live receiver half of a hand-built `SearchStream` until the
    /// one test request that consumes it arrives. `take()`s it out on first
    /// use; a second dispatch against the same rocket instance would panic,
    /// which is fine since each test builds its own instance.
    struct LiveReceiverSlot(
        std::sync::Mutex<
            Option<
                rocket::tokio::sync::mpsc::UnboundedReceiver<SequencedStreamEvent<SearchResult>>,
            >,
        >,
    );

    #[get("/__test/stream")]
    async fn test_stream_route(
        slot: &rocket::State<LiveReceiverSlot>,
    ) -> EventStream<Pin<Box<dyn Stream<Item = Event> + Send>>> {
        let live = slot
            .0
            .lock()
            .expect("slot mutex poisoned")
            .take()
            .expect("live receiver already taken by an earlier dispatch");
        event_stream_from(SearchStream {
            snapshot: Vec::new(),
            live,
        })
    }

    async fn client_with_live_stream() -> (
        Client,
        rocket::tokio::sync::mpsc::UnboundedSender<SequencedStreamEvent<SearchResult>>,
    ) {
        let (tx, rx) = rocket::tokio::sync::mpsc::unbounded_channel();
        let rocket = rocket::build()
            .manage(LiveReceiverSlot(std::sync::Mutex::new(Some(rx))))
            .mount("/", routes![test_stream_route]);
        let client = Client::tracked(rocket)
            .await
            .expect("failed to build test rocket instance for the stream route");
        (client, tx)
    }

    fn sample_result(url: &str) -> SearchResult {
        SearchResult {
            url: url.to_string(),
            title: format!("Title for {url}"),
            description: "a description".to_string(),
            engines: vec!["duckduckgo".to_string()],
            cached: false,
        }
    }

    fn meta_event(sequence: u64) -> SequencedStreamEvent<SearchResult> {
        SequencedStreamEvent {
            sequence: Some(sequence),
            event: StreamEvent::Meta(private_search_engines::StreamMeta {
                order_id: 1,
                canonical: true,
                cached: false,
            }),
        }
    }

    fn results_event(
        sequence: u64,
        position: usize,
        url: &str,
    ) -> SequencedStreamEvent<SearchResult> {
        SequencedStreamEvent {
            sequence: Some(sequence),
            event: StreamEvent::Results(private_search_engines::StreamResults {
                entries: vec![private_search_engines::PositionedResult {
                    position,
                    result: sample_result(url),
                }],
            }),
        }
    }

    fn attribution_event(
        sequence: u64,
        position: usize,
        url: &str,
    ) -> SequencedStreamEvent<SearchResult> {
        SequencedStreamEvent {
            sequence: Some(sequence),
            event: StreamEvent::Attribution(private_search_engines::StreamAttribution {
                position,
                url: url.to_string(),
                engines: vec!["duckduckgo".to_string(), "brave".to_string()],
            }),
        }
    }

    fn engine_event() -> SequencedStreamEvent<SearchResult> {
        SequencedStreamEvent {
            sequence: None,
            event: StreamEvent::Engine(private_search_engines::EngineReport {
                engine: "duckduckgo".to_string(),
                status: private_search_engines::EngineStatus::Ok,
            }),
        }
    }

    fn done_event(sequence: u64) -> SequencedStreamEvent<SearchResult> {
        SequencedStreamEvent {
            sequence: Some(sequence),
            event: StreamEvent::Done(private_search_engines::StreamDone {
                active_order_id: 1,
                canonical_order_id: Some(1),
                next_cursor: 1,
                has_more: false,
            }),
        }
    }

    #[rocket::async_test]
    async fn query_stream_route_serves_named_sse_events_with_ids_in_order() {
        let (client, tx) = client_with_live_stream().await;

        tx.send(meta_event(1)).unwrap();
        tx.send(engine_event()).unwrap();
        tx.send(results_event(2, 0, "https://a.example")).unwrap();
        tx.send(attribution_event(3, 0, "https://a.example"))
            .unwrap();
        tx.send(done_event(4)).unwrap();
        // Dropping the sender closes the channel; combined with the `done`
        // frame above (which `event_stream_from` treats as terminal and
        // stops after), the response completes without needing a timeout.
        drop(tx);

        let res = client.get("/__test/stream").dispatch().await;
        assert_eq!(
            res.headers().get_one("Content-Type"),
            Some("text/event-stream")
        );

        let body = res.into_string().await.expect("expected an SSE body");

        let meta_at = body.find("event:meta\n").expect("missing meta event");
        let engine_at = body.find("event:engine\n").expect("missing engine event");
        let results_at = body.find("event:results\n").expect("missing results event");
        let attribution_at = body
            .find("event:attribution\n")
            .expect("missing attribution event");
        let done_at = body.find("event:done\n").expect("missing done event");

        assert!(
            meta_at < engine_at
                && engine_at < results_at
                && results_at < attribution_at
                && attribution_at < done_at,
            "events out of order:\n{body}"
        );

        // Sequenced (cache-derived) frames carry an `id:`; the id-less
        // cooling/engine report emitted before any cache sequence exists
        // does not. Each event is its own `\n\n`-delimited block (Rocket
        // writes `id`, then `event`, then `data` per event) — so, unlike a
        // naive substring window between two `event:` markers, splitting on
        // the blank-line separator correctly keeps the *next* event's `id:`
        // line (which is written before that event's own `event:` line) out
        // of the block being asserted on.
        let blocks: Vec<&str> = body.split("\n\n").filter(|b| !b.is_empty()).collect();
        let engine_block = blocks
            .iter()
            .find(|b| b.contains("event:engine"))
            .expect("missing engine block");
        assert!(
            !engine_block.contains("id:"),
            "engine frame should have no id:\n{engine_block}"
        );
        let meta_block = blocks
            .iter()
            .find(|b| b.contains("event:meta"))
            .expect("missing meta block");
        assert!(meta_block.contains("id:1\n"), "missing meta id:\n{body}");
        let results_block = blocks
            .iter()
            .find(|b| b.contains("event:results"))
            .expect("missing results block");
        assert!(
            results_block.contains("id:2\n"),
            "missing results id:\n{body}"
        );
        let attribution_block = blocks
            .iter()
            .find(|b| b.contains("event:attribution"))
            .expect("missing attribution block");
        assert!(
            attribution_block.contains("id:3\n"),
            "missing attribution id:\n{body}"
        );
        let done_block = blocks
            .iter()
            .find(|b| b.contains("event:done"))
            .expect("missing done block");
        assert!(done_block.contains("id:4\n"), "missing done id:\n{body}");

        assert!(body.contains(r#""orderId":1"#), "meta payload:\n{body}");
        assert!(
            body.contains(r#""url":"https://a.example""#),
            "results payload:\n{body}"
        );
        assert!(
            body.contains(r#""engines":["duckduckgo","brave"]"#),
            "attribution payload:\n{body}"
        );
        assert!(body.contains(r#""hasMore":false"#), "done payload:\n{body}");
    }

    #[rocket::async_test]
    async fn query_stream_route_forwards_live_events_incrementally_not_as_a_collected_vec() {
        use rocket::tokio::io::AsyncReadExt;
        use rocket::tokio::time::{Duration as TokioDuration, timeout};

        let (client, tx) = client_with_live_stream().await;

        // Send exactly one event and deliberately withhold the terminal
        // `done` frame and keep the sender alive. A "collect the live
        // stream into a Vec, then respond" implementation could never
        // produce any bytes here, since it would have to wait for the
        // channel to end (via `done`/drop) before it had a Vec to send —
        // so successfully reading this one frame under a short timeout
        // directly falsifies that implementation, proving genuine
        // incremental per-event forwarding over HTTP.
        tx.send(meta_event(1)).unwrap();

        let mut res = client.get("/__test/stream").dispatch().await;

        // Rocket writes an event's `id`/`event`/`data` fields as separate
        // lines, which can arrive as separate `read()` chunks — so
        // accumulate until the whole meta block (`\n\n`-terminated) has
        // shown up, entirely under one overall timeout. The channel is
        // still open and nothing terminal has been sent, so this can only
        // succeed if bytes are actually flushed per-event rather than
        // withheld until the live stream ends.
        let accumulate_meta_block = async {
            let mut acc = Vec::new();
            loop {
                let mut buf = [0u8; 512];
                let n = res.read(&mut buf).await.expect("read error");
                assert!(n > 0, "stream ended before the meta frame arrived");
                acc.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&acc).into_owned();
                if text.contains("event:meta") && text.contains("\n\n") {
                    return text;
                }
            }
        };
        let first_block = timeout(TokioDuration::from_secs(2), accumulate_meta_block)
            .await
            .expect(
                "timed out waiting for the meta frame; live events are not forwarded \
                 incrementally (looks collected before responding)",
            );
        assert!(
            first_block.contains("event:meta"),
            "expected the meta frame to arrive before the (withheld) terminal event: {first_block:?}"
        );

        // Now finish the stream so the test client can clean up.
        tx.send(done_event(2)).unwrap();
        drop(tx);
    }
}
