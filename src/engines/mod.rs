use chrono::NaiveDateTime;
use reqwest::Response;
use serde::Serialize;
use sqlx::SqlitePool;
use strum::EnumIter;

use crate::{
    WebSiteResult,
    cache::{self, ResultRow},
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

#[derive(Debug)]
pub enum EngineError<E> {
    ReqwestError(reqwest::Error),
    ParseError(String),
    EngineSpecificError(E),
    Timeout,  // engine timeout
    Cooldown, // prevent engine timeout
    NotAvailable,
}

pub trait Engine {
    type Error: std::fmt::Debug;

    /// get name of engine
    fn name(&self) -> Engines;

    /// get & clear status of engines
    fn is_available(&mut self) -> bool;

    /// search query with engine (must check `is_available()` first!)
    async fn search(
        &mut self,
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<ResultRow>, EngineError<Self::Error>>;
}

#[derive(Debug, Copy, Clone)]
enum EngineState {
    Healthy,
    TimedOut { available_at: NaiveDateTime },
}

#[derive(Debug)]
pub enum FetchError<E> {
    Sqlx(sqlx::Error),
    Engine(EngineError<E>),
}

/// Checks the cache first; if miss, fetches from the engine and stores results.
pub async fn fetch_or_cache_query<E>(
    pool: &SqlitePool,
    engine: &mut E,
    query: &str,
    start: usize,
    count: usize,
) -> Result<Vec<WebSiteResult>, FetchError<E::Error>>
where
    E: Engine + Send + Sync,
{
    let mut website_results = Vec::new();

    let engine_enum = engine.name();
    let engine_id = cache::get_engine_id(pool, engine_enum)
        .await
        .map_err(FetchError::Sqlx)?;

    // Fetch cached results
    let cached_rows = if let Some(query_row) = cache::get_query(pool, query, engine_id)
        .await
        .map_err(FetchError::Sqlx)?
    {
        cache::get_results_for_query(pool, query_row.id)
            .await
            .map_err(FetchError::Sqlx)?
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
            engine: engine.name(),
            cached: true,
        });
    }

    if cached_count < needed_end {
        let missing_start = cached_count;
        let missing_count = needed_end - cached_count;

        let engine_results = if engine.is_available() {
            engine
                .search(query, missing_start, missing_count)
                .await
                .map_err(FetchError::Engine)?
        } else {
            return Err(FetchError::Engine(EngineError::NotAvailable));
        };

        let fetched_at = chrono::Utc::now().naive_utc();
        let _query_id = cache::upsert_query_with_results(
            pool,
            engine_enum,
            query,
            engine_results.clone(),
            fetched_at,
        )
        .await
        .map_err(FetchError::Sqlx)?;

        for cr in &engine_results {
            website_results.push(WebSiteResult {
                url: cr.url.clone(),
                title: cr.title.clone(),
                description: cr.description.clone(),
                engine: engine.name(),
                cached: false,
            });
        }
    }

    Ok(website_results)
}
