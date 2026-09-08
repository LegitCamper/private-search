//! NixOS package search through the same Elasticsearch backend used by
//! search.nixos.org.
//!
//! The backend's Basic credentials are intentionally shipped to every browser
//! in the public frontend bundle. Environment overrides let deployments update
//! them immediately if NixOS rotates the public account or schema version.

use async_trait::async_trait;
use scraper::Html;
use serde::Deserialize;
use serde_json::json;

use super::{encode, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client};

#[derive(Clone)]
pub struct NixPackages;

impl EngineInfo for NixPackages {
    fn name(&self) -> &'static str {
        "NixOS Packages"
    }
}

const PER_PAGE: usize = 20;
const DEFAULT_ENDPOINT: &str = "https://search.nixos.org/backend/latest-51-nixos-unstable/_search";
const DEFAULT_USERNAME: &str = "aWVSALXpZv";
const DEFAULT_PASSWORD: &str = "X8gPHnzL52wFEekuxsfQ9cSh";

fn endpoint() -> String {
    std::env::var("NIXOS_SEARCH_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string())
}

fn credentials() -> (String, String) {
    (
        std::env::var("NIXOS_SEARCH_USERNAME").unwrap_or_else(|_| DEFAULT_USERNAME.to_string()),
        std::env::var("NIXOS_SEARCH_PASSWORD").unwrap_or_else(|_| DEFAULT_PASSWORD.to_string()),
    )
}

fn request_body(query: &str, start: usize) -> serde_json::Value {
    json!({
        "from": start,
        "size": PER_PAGE,
        "query": {
            "bool": {
                "filter": [{"term": {"type": "package"}}],
                "must": [{
                    "multi_match": {
                        "query": query,
                        "fields": [
                            "package_attr_name^9",
                            "package_pname^6",
                            "package_description^1.3",
                            "package_longDescription",
                            "package_programs^7.5"
                        ],
                        "type": "best_fields",
                        "operator": "and"
                    }
                }]
            }
        }
    })
}

#[derive(Deserialize)]
struct SearchResponse {
    hits: Hits,
}

#[derive(Deserialize)]
struct Hits {
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "_source")]
    source: Package,
}

#[derive(Deserialize)]
struct Package {
    package_attr_name: String,
    package_pname: Option<String>,
    package_pversion: Option<String>,
    #[serde(default)]
    package_programs: Vec<String>,
    package_description: Option<String>,
    #[serde(rename = "package_longDescription")]
    package_long_description: Option<String>,
    package_homepage: Option<Homepage>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Homepage {
    One(String),
    Many(Vec<String>),
}

fn plain_text(value: &str) -> String {
    let fragment = Html::parse_fragment(value);
    tidy(&fragment.root_element().text().collect::<Vec<_>>().join(" "))
}

fn homepage(package: &Package) -> Option<&str> {
    match package.package_homepage.as_ref()? {
        Homepage::One(value) => Some(value.as_str()),
        Homepage::Many(values) => values.first().map(String::as_str),
    }
    .map(str::trim)
    .filter(|value| value.starts_with("https://"))
}

fn describe(package: &Package) -> String {
    let blurb = package
        .package_description
        .as_deref()
        .or(package.package_long_description.as_deref())
        .map(plain_text)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "NixOS package".to_string());
    let version = package
        .package_pversion
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let mut metadata = format!("v{version}");
    if let Some(pname) = package
        .package_pname
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != package.package_attr_name)
    {
        metadata.push_str(&format!(" — package {pname}"));
    }
    if !package.package_programs.is_empty() {
        let programs = package
            .package_programs
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        metadata.push_str(&format!(" — programs: {programs}"));
    }
    if let Some(homepage) = homepage(package) {
        metadata.push_str(&format!(" — homepage: {homepage}"));
    }

    truncate(
        &tidy(&format!("{} — {metadata}", truncate(&blurb, 180))),
        300,
    )
}

fn to_results(response: SearchResponse) -> Vec<RawResult> {
    response
        .hits
        .hits
        .into_iter()
        .map(|hit| hit.source)
        .filter(|package| !package.package_attr_name.trim().is_empty())
        .map(|package| RawResult {
            url: format!(
                "https://search.nixos.org/packages?channel=unstable&query={}",
                encode(&package.package_attr_name)
            ),
            title: package.package_attr_name.clone(),
            description: describe(&package),
        })
        .collect()
}

#[cfg(test)]
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "NixOS Packages returned unexpected JSON ({error}); the backend schema may have changed"
        ))
    })?;
    Ok(to_results(response))
}

#[async_trait]
impl SearchEngine for NixPackages {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let (username, password) = credentials();
        let response = browser_client()
            .post(endpoint())
            .basic_auth(username, Some(password))
            .header("content-type", "application/json")
            .body(request_body(query, start).to_string())
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;
        let body = body_or_block(response, "NixOS Packages").await?;
        parse_live_response(&body)
    }
}

fn parse_live_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "NixOS Packages returned unexpected JSON ({error}); the backend schema may have changed"
        ))
    })?;
    Ok(to_results(response))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn body_uses_raw_offset_and_fixed_page_size() {
        let body = request_body("python web", 40);
        assert_eq!(body["from"], 40);
        assert_eq!(body["size"], PER_PAGE);
        assert_eq!(
            body["query"]["bool"]["must"][0]["multi_match"]["query"],
            "python web"
        );
    }

    #[test]
    fn parses_recorded_fixture() {
        let results = parse_response(&fixture("nixpkgs.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|result| {
            result.url.starts_with("https://search.nixos.org/packages?")
                && !result.title.is_empty()
                && !result.description.is_empty()
        }));
        assert_eq!(results[0].title, "python313FreeThreading");
        assert!(results[0].description.contains("v3.13.15"));
    }

    #[test]
    fn strips_html_from_long_descriptions() {
        assert_eq!(plain_text("<p>Hello <strong>Nix</strong></p>"), "Hello Nix");
    }

    #[ignore]
    #[tokio::test]
    async fn live_search_returns_results() {
        let results = NixPackages
            .search_results("python", 0, PER_PAGE)
            .await
            .unwrap();
        assert!(!results.is_empty());
    }
}
