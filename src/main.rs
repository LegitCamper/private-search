use rocket::{
    fs::FileServer,
    response::Redirect,
    serde::{Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

use crate::engines::{Engine, duckduckgo::DuckDuckGo};

#[macro_use]
extern crate rocket;

mod engines;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let _rocket = rocket::build()
        .attach(Template::fairing())
        .mount("/static", FileServer::from("static"))
        .mount("/", routes![index, empty_search, search, query])
        .ignite()
        .await?
        .launch()
        .await?;

    Ok(())
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
    engine: EngineName,
    cached: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
enum EngineName {
    Google,
    DuckDuckGo,
}
