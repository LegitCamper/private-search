//! Hex.pm package search through the public package API.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct HexPm;

impl EngineInfo for HexPm {
    fn name(&self) -> &'static str {
        "Hex"
    }
}

// Hex currently returns up to 100 packages per API page and ignores smaller
// `per_page` values. Slice within that page so arbitrary cache offsets do not
// repeat the same results.
const API_PAGE_SIZE: usize = 100;

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://hex.pm/api/packages?search={}&page={}&sort=recent_downloads",
        encode(query),
        page_number(start, API_PAGE_SIZE)
    )
}

#[derive(Deserialize)]
struct Package {
    name: String,
    html_url: Option<String>,
    latest_version: Option<String>,
    latest_stable_version: Option<String>,
    meta: Option<Metadata>,
    downloads: Option<Downloads>,
}

#[derive(Deserialize)]
struct Metadata {
    description: Option<String>,
    #[serde(default)]
    licenses: Vec<String>,
}

#[derive(Deserialize)]
struct Downloads {
    #[serde(default)]
    all: u64,
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
    let blurb = package
        .meta
        .as_ref()
        .and_then(|meta| meta.description.as_deref())
        .map(tidy)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Hex package".to_string());
    let version = package
        .latest_stable_version
        .as_deref()
        .or(package.latest_version.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let downloads = package.downloads.as_ref().map_or(0, |value| value.all);
    let mut metadata = format!("v{version} — {} downloads", thousands(downloads));
    if let Some(licenses) = package
        .meta
        .as_ref()
        .map(|meta| meta.licenses.as_slice())
        .filter(|licenses| !licenses.is_empty())
    {
        metadata.push_str(&format!(" — {}", licenses.join(", ")));
    }

    truncate(
        &tidy(&format!("{} — {metadata}", truncate(&blurb, 210))),
        300,
    )
}

fn to_results(packages: Vec<Package>, start: usize) -> Vec<RawResult> {
    let within_page = start % API_PAGE_SIZE;
    packages
        .into_iter()
        .skip(within_page)
        .filter(|package| !package.name.trim().is_empty())
        .map(|package| {
            let url = package
                .html_url
                .as_deref()
                .map(str::trim)
                .filter(|value| value.starts_with("https://hex.pm/packages/"))
                .map(str::to_string)
                .unwrap_or_else(|| format!("https://hex.pm/packages/{}", package.name));
            RawResult {
                url,
                title: package.name.clone(),
                description: describe(&package),
            }
        })
        .collect()
}

#[cfg(test)]
fn parse_response(json: &str, start: usize) -> Result<Vec<RawResult>, EngineError> {
    let packages = serde_json::from_str(json).map_err(|error| {
        EngineError::ParseError(format!(
            "Hex returned unexpected JSON ({error}); the API shape may have changed"
        ))
    })?;
    Ok(to_results(packages, start))
}

#[async_trait]
impl SearchEngine for HexPm {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let packages: Vec<Package> = get_json(&build_search_url(query, start), "Hex").await?;
        Ok(to_results(packages, start))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn builds_one_based_page_url() {
        assert_eq!(
            build_search_url("web framework", 100),
            "https://hex.pm/api/packages?search=web%20framework&page=2&sort=recent_downloads"
        );
    }

    #[test]
    fn parses_recorded_fixture() {
        let results = parse_response(&fixture("hexpm.json"), 0).unwrap();
        assert_eq!(results.len(), 20);
        assert!(results.iter().all(|result| {
            result.url.starts_with("https://hex.pm/packages/")
                && !result.title.is_empty()
                && !result.description.is_empty()
        }));
        assert_eq!(results[0].title, "phoenix");
        assert!(results[0].description.contains("downloads"));
    }

    #[test]
    fn applies_the_offset_within_an_api_page() {
        let results = parse_response(&fixture("hexpm.json"), 1).unwrap();
        assert_ne!(results[0].title, "phoenix");
    }

    #[ignore]
    #[tokio::test]
    async fn live_search_returns_results() {
        let results = HexPm.search_results("phoenix", 0, 20).await.unwrap();
        assert!(!results.is_empty());
    }
}
