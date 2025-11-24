use serde::Serialize;
use sqlx::SqlitePool;
use strum::EnumIter;

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

/// Checks the cache first; if miss, fetches from the engine and stores results.
pub async fn fetch_or_cache_query<E>(
    pool: &SqlitePool,
    query: &str,
    start: usize,
    count: usize,
) -> Result<Vec<WebSiteResult>, sqlx::Error>
where
    E: Engine + Send + Sync,
{
    let engine_enum = E::engine();
    let engine_id = cache::get_engine_id(pool, engine_enum).await?;

    // 1. fetch cached results
    let cached_rows = if let Some(query_row) = cache::get_query(pool, query, engine_id).await? {
        cache::get_results_for_query(pool, query_row.id).await?
    } else {
        vec![]
    };

    // 2. figure out which indices are missing
    let mut results_map = cached_rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| (i, r))
        .collect::<std::collections::HashMap<_, _>>();

    let mut missing_start = start;
    let mut missing_count = 0;

    for i in start..start + count {
        if !results_map.contains_key(&i) {
            if missing_count == 0 {
                missing_start = i;
            }
            missing_count += 1;
        }
    }

    // 3. fetch missing from engine
    if missing_count > 0 {
        let engine_results: Vec<EngineResult> = E::search(query, missing_start, missing_count)
            .await
            .unwrap();

        let fetched_at = chrono::Utc::now().naive_utc();
        let _query_id = cache::upsert_query_with_results(
            pool,
            engine_enum,
            query,
            engine_results.clone(),
            fetched_at,
        )
        .await?;

        for (i, r) in engine_results.into_iter().enumerate() {
            results_map.insert(missing_start + i, r.into());
        }
    }

    // 4. build WebSiteResult vec
    let results = (start..start + count)
        .filter_map(|i| results_map.get(&i))
        .map(|r| WebSiteResult {
            url: r.url.clone(),
            title: r.title.clone(),
            description: r.description.clone(),
            engine: engine_enum,
            cached: true, // could track which were fetched vs cached
        })
        .collect();

    Ok(results)
}
