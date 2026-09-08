//! npm — the JavaScript package registry.
//!
//! Answers "is there a package called X" against
//! `registry.npmjs.org/-/v1/search`, the same public JSON API the npmjs.com
//! search box uses. No auth, no key.
//!
//! Two quirks drive the shape of this file:
//!
//! 1. **`from` is a raw offset, not a page number**, so `start` passes
//!    straight through. `size` is clamped server-side to 250.
//!
//! 2. **Past `from = 5000` the API silently wraps back to offset 0** — it
//!    answers HTTP 200 with the *first* page of results rather than an error
//!    or an empty set (verified 2026-09-07: `from=5001` for `text=react`
//!    returns `react` again). A pager that trusted the response would loop
//!    on duplicates forever, so the cap is enforced here on the way out.
//!    Note the cap is on `from` alone: `from=5000&size=250` is fine.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_html, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Npm;

impl EngineInfo for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }
}

/// Results requested when the caller gives no usable `count` hint.
const NPM_DEFAULT_PAGE_SIZE: usize = 20;

/// The registry clamps `size` here server-side; asking for 300 returns 250.
const NPM_MAX_PAGE_SIZE: usize = 250;

/// Largest usable `from`. Beyond this the API wraps to offset 0 instead of
/// reporting exhaustion — see the module docs.
const NPM_MAX_FROM: usize = 5000;

fn page_size(count: usize) -> usize {
    if count == 0 {
        NPM_DEFAULT_PAGE_SIZE
    } else {
        count.min(NPM_MAX_PAGE_SIZE)
    }
}

fn build_search_url(query: &str, start: usize, count: usize) -> String {
    format!(
        "https://registry.npmjs.org/-/v1/search?text={}&size={}&from={start}",
        encode(query),
        page_size(count),
    )
}

/// The human-facing page for a package.
///
/// A scoped name (`@scope/pkg`) must keep both its `@` and its `/` literal —
/// npmjs.com routes on the two path segments, so a percent-encoded slash
/// 404s. [`encode`] leaves both alone while still escaping the characters
/// (`#`, `?`, spaces) that would otherwise truncate or redirect the path.
fn package_url(name: &str) -> String {
    format!("https://www.npmjs.com/package/{}", encode(name))
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    objects: Vec<SearchObject>,
}

#[derive(Deserialize)]
struct SearchObject {
    package: Package,
    #[serde(default)]
    score: Option<Score>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    links: Links,
    #[serde(default)]
    publisher: Option<Publisher>,
}

#[derive(Deserialize, Default)]
struct Links {
    #[serde(default)]
    npm: Option<String>,
}

#[derive(Deserialize)]
struct Publisher {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize)]
struct Score {
    // `final` is a reserved word in Rust, so it can't name the field.
    #[serde(rename = "final", default)]
    final_score: Option<f64>,
}

/// Every result needs a non-empty description, but plenty of real packages
/// ship none (`babel-plugin` in the fixture has `"description": ""`). Build
/// one from whatever metadata the registry did return, falling back through
/// keywords and publisher to the package name itself.
fn describe(object: &SearchObject) -> String {
    let package = &object.package;

    let body = package
        .description
        .as_deref()
        .map(tidy)
        .filter(|d| !d.is_empty())
        .or_else(|| {
            let keywords = tidy(&package.keywords.join(", "));
            (!keywords.is_empty()).then(|| format!("Keywords: {keywords}"))
        })
        .or_else(|| {
            let publisher = package.publisher.as_ref()?.username.as_deref()?;
            Some(format!("npm package published by {}", tidy(publisher)))
        })
        .unwrap_or_else(|| format!("{} on the npm registry", tidy(&package.name)));

    let mut meta = Vec::new();
    if let Some(version) = package
        .version
        .as_deref()
        .map(tidy)
        .filter(|v| !v.is_empty())
    {
        meta.push(format!("v{version}"));
    }
    // Score first — it's what the ranking actually keyed on. Publish date is
    // the consolation prize for the rare object with no `score` block.
    if let Some(score) = object.score.as_ref().and_then(|s| s.final_score) {
        meta.push(format!("score {score:.1}"));
    } else if let Some(date) = package.date.as_deref() {
        // ISO-8601; the time-of-day is noise in a one-line result row.
        if let Some(day) = date.split('T').next().filter(|d| !d.is_empty()) {
            meta.push(format!("published {day}"));
        }
    }

    if meta.is_empty() {
        body
    } else {
        format!("{} — {body}", meta.join(" · "))
    }
}

pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "npm returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(response
        .objects
        .into_iter()
        .filter_map(|object| {
            let title = tidy(&object.package.name);
            if title.is_empty() {
                return None;
            }

            // `links.npm` is the registry's own canonical page URL, but fall
            // back to constructing it so a package missing the link block
            // still yields something openable.
            let url = object
                .package
                .links
                .npm
                .as_deref()
                .map(str::trim)
                .filter(|u| u.starts_with("https://"))
                .map(str::to_string)
                .unwrap_or_else(|| package_url(&title));

            let description = truncate(&describe(&object), 300);

            Some(RawResult {
                url,
                title,
                description,
            })
        })
        .collect())
}

#[async_trait]
impl SearchEngine for Npm {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // Exhaustion, not an error: the API would happily serve page 1 again.
        if start > NPM_MAX_FROM {
            return Ok(Vec::new());
        }

        // Fetched as text rather than via `get_json` so parsing stays a pure
        // function the tests can drive off the recorded fixture. `get_html`
        // still routes through `body_or_block`, so npm's 429 becomes a
        // structured `Blocked` and feeds the cooldown registry.
        let body = get_html(&build_search_url(query, start, count), "npm").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn build_search_url_passes_start_through_as_a_raw_offset() {
        assert_eq!(
            build_search_url("babel-plugin", 0, 20),
            "https://registry.npmjs.org/-/v1/search?text=babel-plugin&size=20&from=0"
        );
        // Not a page number: an offset of 40 is literally `from=40`.
        assert_eq!(
            build_search_url("babel-plugin", 40, 20),
            "https://registry.npmjs.org/-/v1/search?text=babel-plugin&size=20&from=40"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_queries() {
        assert_eq!(
            build_search_url("babel plugin café", 0, 20),
            "https://registry.npmjs.org/-/v1/search?text=babel%20plugin%20caf%C3%A9&size=20&from=0"
        );
    }

    #[test]
    fn page_size_clamps_to_the_registrys_server_side_maximum() {
        assert_eq!(page_size(0), NPM_DEFAULT_PAGE_SIZE);
        assert_eq!(page_size(50), 50);
        assert_eq!(page_size(1000), NPM_MAX_PAGE_SIZE);
    }

    #[test]
    fn package_url_keeps_a_scoped_names_slash_intact() {
        assert_eq!(
            package_url("@stylexjs/babel-plugin"),
            "https://www.npmjs.com/package/@stylexjs/babel-plugin"
        );
        assert_eq!(
            package_url("babel-plugin"),
            "https://www.npmjs.com/package/babel-plugin"
        );
    }

    #[tokio::test]
    async fn search_results_reports_exhaustion_past_the_offset_cap_without_a_request() {
        // Must not error, and must not hit the network — past the cap the
        // API answers 200 with page 1, which would loop the pager.
        let results = Npm
            .search_results("react", NPM_MAX_FROM + 1, 20)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn parse_response_accepts_a_well_formed_empty_result_set() {
        let empty = r#"{"objects":[],"total":0,"time":"2026-09-07T22:45:12.780Z"}"#;
        assert!(parse_response(empty).unwrap().is_empty());
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_the_expected_json() {
        assert!(parse_response("<html>rate limited</html>").is_err());
    }

    // Pinned against a real response recorded 2026-09-07 for `text=babel-plugin`
    // (`tests/fixtures/sites/npm.json`), chosen because it contains both a
    // scoped package and one with an empty description.
    fn npm_fixture() -> String {
        super::super::fixture("npm.json")
    }

    #[test]
    fn parse_response_returns_every_package_from_the_real_fixture() {
        let results = parse_response(&npm_fixture()).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&npm_fixture()).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be renderable; an empty field means a \
             metadata fallback stopped firing"
        );
        assert!(
            results.iter().all(|r| r.url.starts_with("https://")),
            "urls must be absolute and openable, never registry API paths"
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&npm_fixture()).unwrap();
        assert_eq!(
            results[0].url,
            "https://www.npmjs.com/package/babel-plugin-react-compiler"
        );
        assert_eq!(results[0].title, "babel-plugin-react-compiler");
    }

    #[test]
    fn parse_response_gives_a_scoped_package_an_openable_npmjs_url() {
        let results = parse_response(&npm_fixture()).unwrap();
        let scoped = results
            .iter()
            .find(|r| r.title == "@stylexjs/babel-plugin")
            .expect("fixture should contain a scoped package");
        assert_eq!(
            scoped.url,
            "https://www.npmjs.com/package/@stylexjs/babel-plugin"
        );
    }

    #[test]
    fn parse_response_synthesizes_a_description_for_a_package_that_has_none() {
        let results = parse_response(&npm_fixture()).unwrap();
        // `babel-plugin` ships `"description": ""` and no keywords, so the
        // publisher fallback is the only thing left to describe it with.
        let bare = results
            .iter()
            .find(|r| r.title == "babel-plugin")
            .expect("fixture should contain a package with no description");
        assert!(!bare.description.is_empty());
        assert!(bare.description.contains("v1.0.7"));
        assert!(bare.description.contains("yancq"));
    }

    #[test]
    fn parse_response_carries_version_and_score_in_the_description() {
        let results = parse_response(&npm_fixture()).unwrap();
        assert!(results[0].description.starts_with("v1.0.0 · score "));
    }

    #[test]
    fn describe_falls_back_to_keywords_when_there_is_no_description() {
        let json = r#"{"objects":[{"package":{
            "name":"kwonly","version":"2.0.0","description":"",
            "keywords":["cli","tooling"],
            "links":{"npm":"https://www.npmjs.com/package/kwonly"}}}]}"#;
        let results = parse_response(json).unwrap();
        assert_eq!(results[0].description, "v2.0.0 — Keywords: cli, tooling");
    }

    #[test]
    fn describe_uses_the_publish_date_when_the_object_has_no_score() {
        let json = r#"{"objects":[{"package":{
            "name":"dateonly","version":"1.2.3","description":"A thing.",
            "date":"2021-12-28T10:18:42.214Z",
            "links":{"npm":"https://www.npmjs.com/package/dateonly"}}}]}"#;
        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].description,
            "v1.2.3 · published 2021-12-28 — A thing."
        );
    }

    #[test]
    fn parse_response_constructs_a_url_when_the_links_block_is_missing() {
        let json = r#"{"objects":[{"package":{
            "name":"@scope/nolinks","version":"1.0.0","description":"No links."}}]}"#;
        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].url,
            "https://www.npmjs.com/package/@scope/nolinks"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn search_results_live_returns_openable_results_from_the_registry() {
        let results = Npm.search_results("babel-plugin", 0, 20).await.unwrap();
        assert_eq!(results.len(), 20);
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://www.npmjs.com/package/"))
        );
    }

    /// Guards the offset semantics against the API silently wrapping: a
    /// deep `from` must return genuinely different packages, not page 1.
    #[ignore]
    #[tokio::test]
    async fn search_results_live_pagination_does_not_repeat_the_first_page() {
        let page1 = Npm.search_results("react", 0, 20).await.unwrap();
        let page2 = Npm.search_results("react", 20, 20).await.unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "offset paging should advance, not repeat"
        );
    }
}
