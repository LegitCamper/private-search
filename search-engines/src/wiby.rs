//! Wiby — an independent, small-web search engine with a public JSON API.
//!
//! Useful as an unblocked fallback when the larger web engines are cooling
//! down. Its index deliberately favours lightweight, human-authored pages,
//! so it supplements Brave/DDG rather than pretending to replace their
//! breadth. One request per page, no retries, no proxy race.

use async_trait::async_trait;
use serde::Deserialize;

use crate::sites::{encode, get_json, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Wiby;

impl EngineInfo for Wiby {
    fn name(&self) -> &'static str {
        "Wiby"
    }
}

// The public JSON endpoint returns up to 12 rows per page. A sparse page is
// not exhaustion: e.g. `linux` serves 11 while later pages still exist.
const PER_PAGE: usize = 12;

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://wiby.me/json/?q={}&p={}",
        encode(query),
        page_number(start, PER_PAGE)
    )
}

#[derive(Deserialize)]
struct Hit {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Snippet", default)]
    snippet: String,
    #[serde(rename = "Description", default)]
    description: String,
}

fn to_results(hits: Vec<Hit>) -> Vec<RawResult> {
    hits.into_iter()
        .filter(|hit| {
            !hit.title.trim().is_empty()
                && reqwest::Url::parse(&hit.url)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        })
        .map(|hit| RawResult {
            url: hit.url,
            title: tidy(&hit.title),
            description: truncate(
                &tidy(if hit.snippet.trim().is_empty() {
                    &hit.description
                } else {
                    &hit.snippet
                }),
                300,
            ),
        })
        .collect()
}

#[async_trait]
impl SearchEngine for Wiby {
    fn page_size(&self) -> Option<usize> {
        Some(PER_PAGE)
    }

    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let hits = get_json(&build_search_url(query, start), "Wiby").await?;
        Ok(to_results(hits))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn page_urls_are_encoded_and_one_based() {
        assert_eq!(
            build_search_url("a&b=c", 0),
            "https://wiby.me/json/?q=a%26b=c&p=1"
        );
        assert_eq!(
            build_search_url("rust programming", 12),
            "https://wiby.me/json/?q=rust%20programming&p=2"
        );
        assert_eq!(Wiby.next_start(0, 11), 12);
    }

    #[test]
    fn parses_recorded_json() {
        let hits = serde_json::from_str(include_str!("../tests/fixtures/wiby.json")).unwrap();
        let rows = to_results(hits);
        assert_eq!(rows.len(), 12);
        assert!(
            rows.iter()
                .all(|row| !row.title.is_empty() && !row.description.is_empty())
        );
    }

    #[test]
    fn empty_pages_are_exhausted_and_bad_links_are_dropped() {
        assert!(to_results(Vec::new()).is_empty());
        let hits = serde_json::from_str(
            r#"[{"URL":"javascript:alert(1)","Title":"bad"},
                {"URL":"https://example.com","Title":" Good ","Description":" fallback "}]"#,
        )
        .unwrap();
        let rows = to_results(hits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description, "fallback");
    }
}
