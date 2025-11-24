use serde::Serialize;
use strum::EnumIter;

pub mod duckduckgo;
mod google;

#[derive(Debug, Clone, Serialize, EnumIter, sqlx::Type)]
#[serde(crate = "rocket::serde")]
pub enum Engines {
    Google,
    DuckDuckGo,
    Bing,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct EngineResult {
    pub url: String,
    pub title: String,
    pub description: String,
}

pub trait Engine: Sized {
    type Error;

    async fn search(
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<EngineResult>, Self::Error>;
}
