//! Packagist — the package registry every PHP/Composer project installs
//! from. Searching "guzzle" here should hand back the `guzzlehttp/guzzle`
//! package page, not a blog post about it.
//!
//! Uses the unauthenticated `search.json` API (no key, no token). Two quirks
//! shape the code below:
//!
//! * **`total` lies, `next` doesn't.** The `total` count is a fuzzy upper
//!   bound — a query reporting ~94k hits still runs dry long before
//!   `total / per_page` pages. Paging is therefore driven off the `next`
//!   link, which Packagist emits only while another page genuinely exists.
//!   Its *absence* alongside zero results is the exhaustion signal; its
//!   presence alongside zero results is a contradiction we surface as a
//!   parse error rather than silently reporting "no more results" (the same
//!   failure mode a block would produce).
//! * **Hard page cap.** `page` must be in `1..=300`; anything past that is
//!   an HTTP 400 carrying a JSON *error* body, which would deserialize-fail
//!   and look like our bug. We stop at the cap ourselves instead.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Packagist;

impl EngineInfo for Packagist {
    fn name(&self) -> &'static str {
        "Packagist"
    }
}

/// Packagist accepts `per_page` up to 100, but 20 keeps each request cheap
/// and matches the granularity the caller pages at.
const PER_PAGE: usize = 20;

/// Highest `page` the API will accept; 301 and up are rejected with a 400.
const MAX_PAGE: usize = 300;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://packagist.org/search.json?q={}&page={page}&per_page={PER_PAGE}",
        encode(query)
    )
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Package>,
    /// Absolute URL of the following page. Present only when one exists.
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// Already the absolute human-facing packagist.org page — verified
    /// against the live API, but re-checked at runtime before we trust it.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    downloads: u64,
    /// "Favers" is Packagist's word for stargazers.
    #[serde(default)]
    favers: u64,
}

#[async_trait]
impl SearchEngine for Packagist {
    /// `count` is ignored: the page size is fixed so that `start` maps onto a
    /// page boundary.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if page_number(start, PER_PAGE) > MAX_PAGE {
            return Ok(Vec::new());
        }

        let response: SearchResponse =
            get_json(&build_search_url(query, start), "Packagist").await?;
        to_results(response)
    }
}

#[cfg(test)]
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "Packagist returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;
    to_results(response)
}

fn to_results(response: SearchResponse) -> Result<Vec<RawResult>, EngineError> {
    if response.results.is_empty() {
        // A `next` link promising more results on a page that carries none
        // can't both be true. Treating it as exhaustion would hide a shape
        // change behind a plausible-looking "end of results".
        if response.next.is_some() {
            return Err(EngineError::ParseError(
                "Packagist returned no results but advertised a next page; \
                 the API shape may have changed"
                    .into(),
            ));
        }
        return Ok(Vec::new());
    }

    Ok(response
        .results
        .iter()
        .filter(|package| !package.name.trim().is_empty())
        .map(|package| RawResult {
            url: package_url(package),
            title: package.name.clone(),
            description: describe(package),
        })
        .collect())
}

/// Prefers the API's own `url`, but only once it's confirmed to be the
/// human-facing package page — a future API that starts returning its own
/// endpoint there would otherwise hand users an unopenable JSON link.
fn package_url(package: &Package) -> String {
    let candidate = package.url.as_deref().unwrap_or("").trim();
    if candidate.starts_with("https://packagist.org/packages/") {
        return candidate.to_string();
    }
    format!("https://packagist.org/packages/{}", package.name.trim())
}

/// Install count and stars are the relevance signal for a PHP package, so
/// they ride along in the description rather than being dropped.
fn describe(package: &Package) -> String {
    let stats = format!(
        "{} downloads · {} stars",
        thousands(package.downloads),
        thousands(package.favers)
    );

    let summary = tidy(package.description.as_deref().unwrap_or(""));
    let description = if summary.is_empty() {
        // Plenty of real packages ship an empty `description`; the contract
        // still requires a non-empty one, so synthesise from what we have.
        let repository = package.repository.as_deref().unwrap_or("").trim();
        if repository.is_empty() {
            format!("PHP package {} — {stats}", package.name.trim())
        } else {
            format!(
                "PHP package {} — {stats} · {repository}",
                package.name.trim()
            )
        }
    } else {
        format!("{summary} — {stats}")
    };

    truncate(&description, 300)
}

/// Groups digits so 574127483 reads as 574,127,483 — raw nine-digit download
/// counts are unreadable at a glance in a result row.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.char_indices() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_asks_for_the_first_page_when_nothing_has_been_fetched() {
        assert_eq!(
            build_search_url("laravel", 0),
            "https://packagist.org/search.json?q=laravel&page=1&per_page=20"
        );
    }

    #[test]
    fn build_search_url_advances_a_page_per_full_batch_already_held() {
        assert_eq!(
            build_search_url("laravel", PER_PAGE),
            "https://packagist.org/search.json?q=laravel&page=2&per_page=20"
        );
        assert_eq!(
            build_search_url("laravel", PER_PAGE * 4 + 7),
            "https://packagist.org/search.json?q=laravel&page=5&per_page=20"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_in_the_query() {
        assert_eq!(
            build_search_url("café framework", 0),
            "https://packagist.org/search.json?q=caf%C3%A9%20framework&page=1&per_page=20"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_off_the_recorded_api_response() {
        let results = parse_response(&fixture("packagist.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_recorded_api_response() {
        let results = parse_response(&fixture("packagist.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be openable and readable"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://packagist.org/packages/")),
            "urls must be the human-facing package page, never an API url"
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_recorded_api_response() {
        let results = parse_response(&fixture("packagist.json")).unwrap();
        assert_eq!(
            results[0].url,
            "https://packagist.org/packages/symfony/maker-bundle"
        );
        assert_eq!(results[0].title, "symfony/maker-bundle");
    }

    /// `blackbird/category-empty-button` really does ship `"description": ""`
    /// on Packagist — the fixture keeps that case so the synthesised fallback
    /// stays covered.
    #[test]
    fn parse_response_synthesises_a_description_for_a_package_that_has_none() {
        let results = parse_response(&fixture("packagist.json")).unwrap();
        let result = results
            .iter()
            .find(|r| r.title == "blackbird/category-empty-button")
            .expect("fixture should contain the package with an empty description");

        assert!(result.description.contains("5,321 downloads"));
        assert!(result.description.contains("6 stars"));
        assert!(result.description.contains("github.com"));
    }

    #[test]
    fn parse_response_carries_downloads_and_stars_alongside_a_real_description() {
        let results = parse_response(&fixture("packagist.json")).unwrap();
        assert!(
            results[0]
                .description
                .starts_with("Symfony Maker helps you")
        );
        assert!(results[0].description.contains(" downloads · "));
        assert!(results[0].description.ends_with(" stars"));
    }

    #[test]
    fn parse_response_truncates_a_long_description() {
        let json = format!(
            r#"{{"results":[{{"name":"a/b","description":"{}","url":"https://packagist.org/packages/a/b","downloads":1,"favers":0}}],"total":1}}"#,
            "x".repeat(500)
        );
        let results = parse_response(&json).unwrap();
        assert_eq!(results[0].description.chars().count(), 301); // 300 + ellipsis
    }

    #[test]
    fn parse_response_falls_back_to_a_built_url_when_the_api_url_is_not_a_package_page() {
        let json = r#"{"results":[{"name":"vendor/pkg","description":"d","url":"https://packagist.org/search.json?q=x","downloads":1,"favers":2}],"total":1}"#;
        let results = parse_response(json).unwrap();
        assert_eq!(results[0].url, "https://packagist.org/packages/vendor/pkg");
    }

    /// Exhaustion, not failure: a query past its last page really does come
    /// back as a well-formed `{"results":[],"total":0}` with no `next`.
    #[test]
    fn parse_response_treats_a_well_formed_empty_page_as_exhaustion() {
        let results = parse_response(r#"{"results":[],"total":0}"#).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn parse_response_rejects_an_empty_page_that_still_advertises_a_next_page() {
        let json =
            r#"{"results":[],"total":9,"next":"https://packagist.org/search.json?q=x&page=2"}"#;
        assert!(matches!(
            parse_response(json),
            Err(EngineError::ParseError(_))
        ));
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_the_expected_json() {
        assert!(matches!(
            parse_response("<html>rate limited</html>"),
            Err(EngineError::ParseError(_))
        ));
    }

    #[test]
    fn thousands_groups_digits_without_touching_short_numbers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(574_127_483), "574,127,483");
    }

    #[ignore]
    #[tokio::test]
    async fn packagist_returns_live_results_and_stops_past_the_page_cap() {
        let engine = Packagist;

        let page1 = engine.search_results("laravel", 0, 20).await.unwrap();
        assert!(!page1.is_empty());

        let page2 = engine
            .search_results("laravel", PER_PAGE, 20)
            .await
            .unwrap();
        assert!(
            page2.iter().all(|r| !page1.iter().any(|p| p.url == r.url)),
            "pagination should advance, not repeat page 1"
        );

        // Past MAX_PAGE the API 400s; we must short-circuit instead.
        let beyond = engine
            .search_results("laravel", PER_PAGE * MAX_PAGE, 20)
            .await
            .unwrap();
        assert!(beyond.is_empty());
    }
}
