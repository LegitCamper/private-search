use rocket::{
    Request, Response,
    fairing::{Fairing, Info, Kind},
    fs::FileServer,
    response::Redirect,
    serde::{Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

use crate::engines::{Engine, Engines, duckduckgo::DuckDuckGo};

#[macro_use]
extern crate rocket;

mod cache;
mod engines;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let db_conn = cache::init();

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
async fn query<'a>(
    query: &str,
    start: usize,
    count: usize,
) -> Result<Json<Vec<WebSiteResult>>, String> {
    // // Validate count
    // if count > 25 {
    //     return Err("maximum allowed count is 25".into());
    // }

    // match DuckDuckGo::search(query, start, count).await {
    //     Ok(results) => Ok(Json(results)),
    //     Err(e) => {
    //         let err = format!("Engine Error: {:?}", e);
    //         Err(err)
    //     }
    // }
    //
    Err("Not working".into())
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct WebSiteResult {
    url: String,
    title: String,
    description: String,
    engine: Engines,
    cached: bool,
}
