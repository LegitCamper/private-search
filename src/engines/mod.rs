use serde::Serialize;
use sqlx::SqlitePool;
use strum::{Display, EnumIter};

use crate::{
    WebSiteResult,
    cache::{self, EngineResultRow},
};

pub mod duckduckgo;
mod google;

#[derive(Debug, Copy, Clone, Serialize, EnumIter, sqlx::Type)]
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

impl Into<EngineResultRow> for EngineResult {
    fn into(self) -> EngineResultRow {
        EngineResultRow {
            url: self.url,
            title: self.title,
            description: self.description,
        }
    }
}

pub trait Engine: Sized {
    type Error: std::fmt::Debug;

    fn engine() -> Engines;

    async fn search(
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<EngineResult>, Self::Error>;
}

#[derive(Debug)]
pub enum FetchCacheError<E: Engine> {
    Sqlx(sqlx::Error),
    Engine(E::Error),
}

/// Checks the cache first; if miss, fetches from the engine and stores results.
pub async fn fetch_or_cache_query<E>(
    pool: &SqlitePool,
    query: &str,
    start: usize,
    count: usize,
) -> Result<Vec<WebSiteResult>, FetchCacheError<E>>
where
    E: Engine + Send + Sync,
{
    let mut website_results = Vec::new();

    let engine_enum = E::engine();
    let engine_id = cache::get_engine_id(pool, engine_enum)
        .await
        .map_err(FetchCacheError::Sqlx)?;

    // Fetch cached results
    let mut cached_rows = if let Some(query_row) = cache::get_query(pool, query, engine_id)
        .await
        .map_err(FetchCacheError::Sqlx)?
    {
        cache::get_results_for_query(pool, query_row.id)
            .await
            .map_err(FetchCacheError::Sqlx)?
    } else {
        Vec::new()
    };

    let cached_count = cached_rows.len();
    let needed_end = start + count;

    let start = start.min(cached_count);
    let end = cached_count.min(needed_end);

    for cr in &cached_rows[start..end] {
        website_results.push(WebSiteResult {
            url: cr.url.clone(),
            title: cr.title.clone(),
            description: cr.description.clone(),
            engine: E::engine(),
            cached: true,
        });
    }

    if cached_count < needed_end {
        let missing_start = cached_count;
        let missing_count = needed_end - cached_count;

        let engine_results = E::search(query, missing_start, missing_count)
            .await
            .map_err(FetchCacheError::Engine)?;

        let fetched_at = chrono::Utc::now().naive_utc();
        let _query_id = cache::upsert_query_with_results(
            pool,
            engine_enum,
            query,
            engine_results.clone(),
            fetched_at,
        )
        .await
        .map_err(FetchCacheError::Sqlx)?;

        for cr in &engine_results {
            website_results.push(WebSiteResult {
                url: cr.url.clone(),
                title: cr.title.clone(),
                description: cr.description.clone(),
                engine: E::engine(),
                cached: false,
            });
        }
    }

    Ok(website_results)
}
