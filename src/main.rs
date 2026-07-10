use rocket::{
    Request, Response,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    response::Redirect,
    serde::{Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

use private_search_engines::{
    FetchError, ImageEngines, ImageResult, ImageSearchBuilder, SearchBuilder, SearchEngines,
    SearchResponse, SearchResult, init_db,
};

#[macro_use]
extern crate rocket;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    _ = init_db();
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

    #[cfg(debug_assertions)]
    println!("res: {:?}", results);

    Ok(Json(results))
}
