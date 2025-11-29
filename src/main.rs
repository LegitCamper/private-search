use std::{ops::DerefMut, sync::Arc};

use rocket::{
    Request, Response, State,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    futures::lock::Mutex,
    response::Redirect,
    serde::json::Json,
};
use rocket_dyn_templates::{Template, context};

use private_search_engines::{Engines, FetchError, SearchResult, fetch_or_cache_query, search_all};

#[macro_use]
extern crate rocket;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let _rocket = rocket::build()
        .attach(Template::fairing())
        .attach(CacheFairing)
        .mount("/static", FileServer::from("static"))
        .mount("/", routes![index, empty_search, search, query])
        .ignite()
        .await?
        .launch()
        .await?;

    Ok(())
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

#[allow(unused_variables)]
#[get("/search?<q>")]
fn search(q: &str) -> Template {
    Template::render(
        "search",
        context! {
            title: "Search",
        },
    )
}

#[get("/query?<query>&<start>&<count>")]
async fn query(query: &str, start: usize, count: usize) -> Result<Json<Vec<SearchResult>>, String> {
    // Validate count
    if count > 25 {
        return Err("maximum allowed count is 25".into());
    }

    println!("query: {}, start: {}, count: {}", query, start, count);

    let results = search_all(
        String::from(query),
        vec![Engines::Brave, Engines::DuckDuckGo],
    )
    .await
    .map_err(|e| {
        match e {
            FetchError::Sqlx(error) => {
                eprint!("Sql Error: {}", error)
            }
            FetchError::Engine(error) => {
                eprint!("Engine Error: {:?}", error)
            }
            FetchError::Timeouts => {
                eprint!("Some Engines timed out")
            }
            FetchError::AllEnginesFailed => {
                eprint!("All Engines Failed")
            }
        }
        "Query Error".to_string()
    })?;

    println!("res: {:?}", results);

    Ok(Json(results))
}
