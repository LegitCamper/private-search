use std::time::Duration;

use rocket::{
    Orbit, Request, Response, Rocket,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    http::Status,
    response::Redirect,
    serde::{Serialize, json::Json},
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

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    init_db().await;

    let static_dir = resolve_dir("STATIC_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/static"));
    let template_dir = resolve_dir(
        "TEMPLATE_DIR",
        concat!(env!("CARGO_MANIFEST_DIR"), "/templates"),
    );

    let cache_clean_interval = resolve_secs("CACHE_CLEAN_INTERVAL_SECS", 60 * 60); // hourly
    let cache_max_age = resolve_secs("CACHE_MAX_AGE_SECS", 7 * 24 * 60 * 60); // 7 days

    let figment = rocket::Config::figment().merge(("template_dir", template_dir));

    let _rocket = rocket::custom(figment)
        .attach(Template::fairing())
        .attach(CacheFairing)
        .attach(CacheCleanupFairing {
            interval: cache_clean_interval,
            max_age: cache_max_age,
        })
        .manage(RateLimiter::default())
        .mount("/static", FileServer::from(static_dir))
        .mount("/", routes![index, empty_search, search, query, health])
        .ignite()
        .await?
        .launch()
        .await?;

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
            title: "Homepage"
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

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct TabFlags {
    general: bool,
    images: bool,
}

#[allow(unused_variables)]
#[get("/search?<t>&<q>")]
fn search(t: Option<String>, q: &str) -> Template {
    Template::render(
        "search",
        context! {
            title: "Search",
        },
    )
}

#[derive(Serialize, Debug)]
#[serde(crate = "rocket::serde")]
pub enum QueryResults {
    General(SearchResponse<SearchResult>),
    Images(SearchResponse<ImageResult>),
}

#[get("/query?<tab>&<query>&<start>&<count>")]
async fn query(
    _limit: RateLimited,
    tab: &str,
    query: &str,
    start: usize,
    count: usize,
) -> Result<Json<QueryResults>, String> {
    // Validate count
    if count > 25 {
        return Err("maximum allowed count is 25".into());
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
        _ => return Err("Unknown Tab query requested".into()),
    }
    .map_err(|e| {
        match &e {
            FetchError::Sqlx(error) => log::error!("cache db error: {error}"),
            FetchError::Engine(error) => log::warn!("engine error: {error:?}"),
            FetchError::Timeouts => log::warn!("query timed out: tab={tab} query={query:?}"),
            FetchError::AllEnginesFailed => {
                log::error!("all engines failed: tab={tab} query={query:?}")
            }
        }
        "Query Error".to_string()
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
