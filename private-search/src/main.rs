use std::time::Duration;

use rocket::{
    Build, Orbit, Request, Response, Rocket,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    http::Status,
    response::Redirect,
    serde::{Deserialize, Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

use private_search_engines::{
    FetchError, ImageEngines, ImageResult, ImageSearchBuilder, SearchBuilder, SearchEngines,
    SearchResponse, SearchResult, init_db,
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
    let cache_max_age = resolve_secs("CACHE_MAX_AGE_SECS", 7 * 24 * 60 * 60); // 7 days

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
        .mount("/", routes![index, empty_search, search, query, health])
}

#[rocket::main]
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
                        log::info!("cache cleanup: purged {purged} stale quer{}", if purged == 1 { "y" } else { "ies" });
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
        if req.uri().path().starts_with("/static/")
            || req.uri().path().starts_with("/search")
            || req.uri().path() == "/"
        {
            res.set_header(rocket::http::Header::new(
                "Cache-Control",
                "public, max-age=86400 ",
            ));
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
            .engines([SearchEngines::Brave, SearchEngines::DuckDuckGo])
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
    .map_err(|e| {
        let status = match &e {
            FetchError::Cache(_) => Status::InternalServerError,
            FetchError::AllEnginesFailed => Status::BadGateway,
        };
        match &e {
            FetchError::Cache(cache_err) => log::error!("cache db error: {cache_err}"),
            FetchError::AllEnginesFailed => {
                log::error!("all engines failed: tab={tab} query={query:?}")
            }
        }
        api_error(status, "query failed")
    })?;

    log::debug!("query ok: tab={tab} query={query:?} results={}", results_len(&results));

    Ok(Json(results))
}

fn results_len(results: &QueryResults) -> usize {
    match results {
        QueryResults::General(r) => r.results.len(),
        QueryResults::Images(r) => r.results.len(),
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
}
