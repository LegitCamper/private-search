//! Maven Central artifact search via Sonatype's documented Solr API.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct MavenCentral;

impl EngineInfo for MavenCentral {
    fn name(&self) -> &'static str {
        "Maven Central"
    }
}

const PER_PAGE: usize = 20;
const MAX_RESULTS: usize = 10_000;

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://search.maven.org/solrsearch/select?q={}&rows={PER_PAGE}&start={start}&wt=json",
        encode(query)
    )
}

#[derive(Deserialize)]
struct SearchResponse {
    response: SolrResponse,
}

#[derive(Deserialize)]
struct SolrResponse {
    docs: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    g: String,
    a: String,
    #[serde(rename = "latestVersion")]
    latest_version: Option<String>,
    p: Option<String>,
    #[serde(rename = "versionCount", default)]
    version_count: u64,
}

fn describe(artifact: &Artifact) -> String {
    let coordinate = format!("{}:{}", artifact.g, artifact.a);
    let version = artifact
        .latest_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let packaging = artifact
        .p
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("artifact");

    truncate(
        &tidy(&format!(
            "{coordinate} — latest {version} — {packaging} — {} published versions",
            artifact.version_count
        )),
        300,
    )
}

fn to_results(response: SearchResponse) -> Vec<RawResult> {
    response
        .response
        .docs
        .into_iter()
        .filter(|artifact| !artifact.g.trim().is_empty() && !artifact.a.trim().is_empty())
        .map(|artifact| RawResult {
            url: format!(
                "https://central.sonatype.com/artifact/{}/{}",
                artifact.g, artifact.a
            ),
            title: format!("{}:{}", artifact.g, artifact.a),
            description: describe(&artifact),
        })
        .collect()
}

#[cfg(test)]
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "Maven Central returned unexpected JSON ({error}); the API shape may have changed"
        ))
    })?;
    Ok(to_results(response))
}

#[async_trait]
impl SearchEngine for MavenCentral {
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
            get_json(&build_search_url(query, start), "Maven Central").await?;
        Ok(to_results(response))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn builds_offset_paginated_url() {
        assert_eq!(
            build_search_url("json parser", 20),
            "https://search.maven.org/solrsearch/select?q=json%20parser&rows=20&start=20&wt=json"
        );
    }

    #[test]
    fn parses_recorded_fixture() {
        let results = parse_response(&fixture("maven.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|result| {
            result
                .url
                .starts_with("https://central.sonatype.com/artifact/")
                && !result.title.is_empty()
                && !result.description.is_empty()
        }));
        assert_eq!(
            results[0].title,
            "org.openidentityplatform.commons.selfservice:json"
        );
        assert!(results[0].description.contains("latest 2.3.0"));
    }

    #[tokio::test]
    async fn rejects_offsets_past_the_safety_cap_without_fetching() {
        let results = MavenCentral
            .search_results("json", MAX_RESULTS, PER_PAGE)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn live_search_returns_results() {
        let results = MavenCentral
            .search_results("json", 0, PER_PAGE)
            .await
            .unwrap();
        assert!(!results.is_empty());
    }
}
