use rocket::{
    fs::FileServer,
    response::Redirect,
    serde::{Serialize, json::Json},
};
use rocket_dyn_templates::{Template, context};

#[macro_use]
extern crate rocket;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    // let _ = tokio::spawn(async {
    // })
    // .await;

    let _rocket = rocket::build()
        .attach(Template::fairing())
        .mount("/static", FileServer::from("static"))
        .mount("/", routes![index, search, search_query, query])
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

#[post("/search")]
fn search_query() -> Redirect {
    Redirect::to("/search")
}

#[get("/search")]
fn search() -> Template {
    Template::render(
        "search",
        context! {
            title: "Search",
            query_id: 1,
        },
    )
}

#[get("/query")]
fn query<'a>() -> Json<Vec<WebSiteResult>> {
    Json(vec![WebSiteResult::default(); 20])
}

impl Default for WebSiteResult {
    fn default() -> Self {
        WebSiteResult {
            url: String::from("https://example.com"),
            title: String::from("Some Example Site"),
            description: String::from(
                "This is some description of a really long description that could say something like the quick brown fox jumped up and over the moon and never came back. The End",
            ),
            engine: Engine::Google,
            cached: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
struct WebSiteResult {
    url: String,
    title: String,
    description: String,
    engine: Engine,
    cached: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
enum Engine {
    Google,
}
