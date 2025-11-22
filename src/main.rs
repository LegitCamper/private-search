use rocket::{fs::FileServer, response::Redirect};
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
        .mount("/", routes![index, search, search_query])
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
            articles: [0, 1, 2, 3, 4, 5],
        },
    )
}

#[derive(Debug, Clone, Copy)]
enum Engine {
    Google,
}

#[derive(Debug, Clone)]
struct WebSiteResult {
    url: String,
    title: String,
    description: String,
    engine: Engine,
    cached: bool,
}
