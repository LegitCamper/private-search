//! Go package search through pkg.go.dev's public JSON API.
//!
//! The API paginates with opaque tokens while the shared engine trait uses an
//! offset. Requesting the first `start + 20` results and slicing locally gives
//! deterministic offset pagination for the API's documented 100-result window.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct GoPkg;

impl EngineInfo for GoPkg {
    fn name(&self) -> &'static str {
        "pkg.go.dev"
    }
}

const PER_PAGE: usize = 20;
const MAX_RESULTS: usize = 100;

fn build_search_url(query: &str, start: usize) -> String {
    let limit = (start + PER_PAGE).min(MAX_RESULTS);
    format!(
        "https://pkg.go.dev/v1/search?q={}&limit={limit}",
        encode(query)
    )
}

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    #[serde(rename = "packagePath")]
    package_path: String,
    #[serde(rename = "modulePath")]
    module_path: Option<String>,
    version: Option<String>,
    synopsis: Option<String>,
}

fn describe(package: &Package) -> String {
    let synopsis = tidy(package.synopsis.as_deref().unwrap_or_default());
    let mut metadata = package
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|version| format!("version {version}"))
        .unwrap_or_else(|| "Go package".to_string());
    if let Some(module) = package
        .module_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        metadata.push_str(&format!(" — module {module}"));
    }

    if synopsis.is_empty() {
        truncate(&metadata, 300)
    } else {
        truncate(
            &tidy(&format!("{} — {metadata}", truncate(&synopsis, 210))),
            300,
        )
    }
}

fn to_results(response: SearchResponse, start: usize) -> Vec<RawResult> {
    response
        .items
        .into_iter()
        .skip(start)
        .take(PER_PAGE)
        .filter(|package| !package.package_path.trim().is_empty())
        .map(|package| RawResult {
            url: format!("https://pkg.go.dev/{}", package.package_path),
            title: package.package_path.clone(),
            description: describe(&package),
        })
        .collect()
}

#[cfg(test)]
fn parse_response(json: &str, start: usize) -> Result<Vec<RawResult>, EngineError> {
    let response = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "pkg.go.dev returned unexpected JSON ({error}); the API shape may have changed"
        ))
    })?;
    Ok(to_results(response, start))
}

#[async_trait]
impl SearchEngine for GoPkg {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if start >= MAX_RESULTS {
            return Ok(Vec::new());
        }

        let response: SearchResponse =
            get_json(&build_search_url(query, start), "pkg.go.dev").await?;
        Ok(to_results(response, start))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn requests_enough_rows_to_slice_to_the_offset() {
        assert_eq!(
            build_search_url("http client", 20),
            "https://pkg.go.dev/v1/search?q=http%20client&limit=40"
        );
        assert_eq!(
            build_search_url("http", 95),
            "https://pkg.go.dev/v1/search?q=http&limit=100"
        );
    }

    #[test]
    fn parses_recorded_fixture() {
        let results = parse_response(&fixture("gopkg.json"), 0).unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|result| {
            result.url.starts_with("https://pkg.go.dev/")
                && !result.title.is_empty()
                && !result.description.is_empty()
        }));
        assert_eq!(results[0].title, "net/http");
        assert!(results[0].description.contains("HTTP client and server"));
    }

    #[test]
    fn slices_from_the_requested_offset() {
        let results = parse_response(&fixture("gopkg.json"), 5).unwrap();
        assert_eq!(results[0].title, "github.com/Azure/go-autorest/autorest");
    }

    #[ignore]
    #[tokio::test]
    async fn live_search_returns_results() {
        let results = GoPkg.search_results("http", 0, PER_PAGE).await.unwrap();
        assert!(!results.is_empty());
    }
}
