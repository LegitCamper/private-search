//! NuGet package search through nuget.org's V3 SearchQueryService.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct NuGet;

impl EngineInfo for NuGet {
    fn name(&self) -> &'static str {
        "NuGet"
    }
}

const PER_PAGE: usize = 20;
const MAX_START: usize = 3_000;

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://azuresearch-usnc.nuget.org/query?q={}&skip={start}&take={PER_PAGE}&prerelease=false&semVerLevel=2.0.0",
        encode(query)
    )
}

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    version: Option<String>,
    description: Option<String>,
    summary: Option<String>,
    title: Option<String>,
    #[serde(rename = "projectUrl")]
    project_url: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(rename = "totalDownloads", default)]
    total_downloads: u64,
    #[serde(default)]
    verified: bool,
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn describe(package: &Package) -> String {
    let blurb = [
        package.description.as_deref(),
        package.summary.as_deref(),
        package.title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(tidy)
    .find(|value| !value.is_empty())
    .unwrap_or_else(|| "NuGet package".to_string());

    let version = package
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let mut metadata = format!(
        "v{version} — {} downloads{}",
        thousands(package.total_downloads),
        if package.verified {
            " — verified"
        } else {
            ""
        }
    );
    if !package.authors.is_empty() {
        metadata.push_str(&format!(" — by {}", package.authors.join(", ")));
    }
    if let Some(project_url) = package
        .project_url
        .as_deref()
        .map(str::trim)
        .filter(|value| value.starts_with("https://"))
    {
        metadata.push_str(&format!(" — project: {project_url}"));
    }
    if !package.tags.is_empty() {
        metadata.push_str(&format!(" — tags: {}", package.tags.join(", ")));
    }

    truncate(
        &tidy(&format!("{} — {metadata}", truncate(&blurb, 170))),
        300,
    )
}

fn to_results(response: SearchResponse) -> Vec<RawResult> {
    response
        .data
        .into_iter()
        .filter(|package| !package.id.trim().is_empty())
        .map(|package| RawResult {
            url: format!("https://www.nuget.org/packages/{}", package.id),
            title: package.id.clone(),
            description: describe(&package),
        })
        .collect()
}

#[cfg(test)]
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "NuGet returned unexpected JSON ({error}); the API shape may have changed"
        ))
    })?;
    Ok(to_results(response))
}

#[async_trait]
impl SearchEngine for NuGet {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if start >= MAX_START {
            return Ok(Vec::new());
        }

        let response: SearchResponse = get_json(&build_search_url(query, start), "NuGet").await?;
        Ok(to_results(response))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn builds_skip_take_paginated_url() {
        assert_eq!(
            build_search_url("json parser", 40),
            "https://azuresearch-usnc.nuget.org/query?q=json%20parser&skip=40&take=20&prerelease=false&semVerLevel=2.0.0"
        );
    }

    #[test]
    fn parses_recorded_fixture() {
        let results = parse_response(&fixture("nuget.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|result| {
            result.url.starts_with("https://www.nuget.org/packages/")
                && !result.title.is_empty()
                && !result.description.is_empty()
        }));
        assert_eq!(results[0].title, "Newtonsoft.Json");
        assert!(results[0].description.contains("verified"));
    }

    #[test]
    fn formats_download_counts() {
        assert_eq!(thousands(9_126_730_694), "9,126,730,694");
    }

    #[ignore]
    #[tokio::test]
    async fn live_search_returns_results() {
        let results = NuGet.search_results("json", 0, PER_PAGE).await.unwrap();
        assert!(!results.is_empty());
    }
}
