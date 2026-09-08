//! PyPI — the Python Package Index.
//!
//! Answers "is there a Python package called X, and what is it" so that
//! searching `pangolin` surfaces the `pangolin` package itself rather than a
//! blog post about pangolins.
//!
//! # Why this is an exact-name lookup and not a search
//!
//! PyPI deliberately has no public search API: the XML-RPC `search` method
//! was disabled in 2021 after sustained abuse, and there has never been a
//! JSON equivalent. That leaves two options, and only one of them works from
//! a server:
//!
//! 1. **Scrape `https://pypi.org/search/?q=…`.** Verified dead on
//!    2026-09-07: the URL answers HTTP **200** with a Fastly bot-management
//!    interstitial (`<title>Client Challenge</title>`, assets under
//!    `/_fs-ch-…/`) that solves itself only by running JavaScript. The
//!    `a.package-snippet` rows never arrive. Because it is a 200 and not a
//!    403, the shared [`crate::body_or_block`] classifier cannot see it — it
//!    would have parsed to zero results forever and looked like "PyPI has no
//!    packages named that".
//! 2. **The per-project JSON API, `https://pypi.org/pypi/<name>/json`.**
//!    Unauthenticated, uncached-friendly, no challenge, no observed rate
//!    limit, and a clean 404 for names that don't exist.
//!
//! So this engine takes option 2: it treats the query as a candidate package
//! name (PEP 503 normalization — lowercased, runs of `-`, `_`, `.` and
//! whitespace collapsed to a single `-`) and returns at most one result. That
//! is narrower than a real search, but it is exactly the question this
//! project exists to answer, and it degrades honestly: an unknown name is an
//! empty result set, never a fabricated one.
//!
//! Consequently there is only ever one page: any `start > 0` is exhaustion.

use async_trait::async_trait;
use serde::Deserialize;

use crate::{EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client};

use super::{encode, page_number, tidy, truncate};

#[derive(Clone)]
pub struct PyPi;

impl EngineInfo for PyPi {
    fn name(&self) -> &'static str {
        "PyPI"
    }
}

/// An exact-name lookup yields at most one hit, so page 2 is always empty.
const PER_PAGE: usize = 1;

const HOST: &str = "https://pypi.org";

/// Characters that would break out of the `/pypi/<name>/json` path segment
/// rather than being escaped by [`encode`], which is tuned for query strings
/// and leaves `/` alone. A name containing any of them is not a package name
/// and must never be turned into a request.
const PATH_BREAKING: [char; 5] = ['/', '\\', '?', '#', '%'];

/// Reduces a free-text query to a PEP 503-normalized distribution name:
/// lowercase, with every run of `-`, `_`, `.` or whitespace collapsed to one
/// `-`. PyPI applies the same normalization server-side, so `Zope.Interface`
/// and `zope-interface` reach the same project.
///
/// Whitespace is folded into `-` as well (beyond PEP 503) so that a typed
/// query like `python dateutil` still finds `python-dateutil`. Non-ASCII is
/// left in place for [`build_lookup_url`] to percent-encode; PyPI answers 404
/// for it, which is the correct answer.
///
/// Returns `None` when nothing usable is left, or when the result contains a
/// character that would escape the URL path.
fn candidate_name(query: &str) -> Option<String> {
    let mut name = String::with_capacity(query.len());
    let mut pending_separator = false;

    for ch in query.chars() {
        if ch == '-' || ch == '_' || ch == '.' || ch.is_whitespace() {
            // Only emit a separator once we know a real character follows,
            // which drops leading and trailing runs for free.
            pending_separator = !name.is_empty();
            continue;
        }
        if pending_separator {
            name.push('-');
            pending_separator = false;
        }
        name.extend(ch.to_lowercase());
    }

    if name.is_empty() || name.contains(PATH_BREAKING) {
        return None;
    }
    Some(name)
}

fn build_lookup_url(name: &str) -> String {
    format!("{HOST}/pypi/{}/json", encode(name))
}

/// A real project response always carries an `info` object. The 404 body is
/// `{"message": "Not Found"}` — valid JSON, so it deserializes fine and lands
/// as "no project" rather than as a parse failure.
///
/// This is the JSON counterpart of the HTML engines' results-marker check:
/// if PyPI ever puts the same Fastly challenge in front of the JSON API, the
/// body becomes HTML and this returns `false`, so the caller raises a
/// [`EngineError::ParseError`] instead of silently reporting "no such
/// package" for every query.
fn looks_like_project_json(body: &str) -> bool {
    body.trim_start().starts_with('{')
}

#[derive(Deserialize)]
struct ProjectResponse {
    info: Option<ProjectInfo>,
}

// Every field is optional: PyPI serializes unset metadata as JSON `null`
// (`author` and `home_page` are null even on popular packages), and a
// `#[derive(Deserialize)]` on `String` would reject that outright.
#[derive(Deserialize)]
struct ProjectInfo {
    name: Option<String>,
    version: Option<String>,
    summary: Option<String>,
    /// Human-facing project page, e.g. `https://pypi.org/project/pangolin/`.
    package_url: Option<String>,
    project_url: Option<String>,
    author: Option<String>,
    author_email: Option<String>,
}

/// Turns whatever PyPI put in `package_url` into an absolute `https://`
/// project URL. It is absolute today, but the field has historically been a
/// relative `/project/<name>/` path, and a relative href in a results list is
/// a link the user cannot open.
fn absolutize(href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if let Some(rest) = href.strip_prefix("http://") {
        return Some(format!("https://{rest}"));
    }
    if href.starts_with("https://") {
        return Some(href.to_string());
    }
    if href.starts_with('/') {
        return Some(format!("{HOST}{href}"));
    }
    None
}

fn nonempty(field: &Option<String>) -> Option<&str> {
    field.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Builds a description when the package has no summary — a real and common
/// case for packages published straight from a bare `setup.py`. An empty
/// `description` would violate this crate's result contract.
fn synthesize_description(name: &str, version: Option<&str>, author: Option<&str>) -> String {
    let mut description = match version {
        Some(version) => format!("Version {version} of the Python package {name}, on PyPI"),
        None => format!("The Python package {name}, on PyPI"),
    };
    if let Some(author) = author {
        description.push_str(&format!(", by {author}"));
    }
    description.push('.');
    description
}

pub fn parse_response(body: &str) -> Result<Vec<RawResult>, EngineError> {
    if !looks_like_project_json(body) {
        return Err(EngineError::ParseError(
            "PyPI returned a non-JSON body; the project API may now be behind \
             the same bot challenge as the search page"
                .into(),
        ));
    }

    let response: ProjectResponse = serde_json::from_str(body).map_err(|e| {
        EngineError::ParseError(format!(
            "PyPI returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    // No `info` means the "Not Found" body: a real absence, not an error.
    let Some(info) = response.info else {
        return Ok(Vec::new());
    };

    let Some(name) = nonempty(&info.name) else {
        return Err(EngineError::ParseError(
            "PyPI project response had no name; the API shape may have changed".into(),
        ));
    };
    let version = nonempty(&info.version);
    let author = nonempty(&info.author).or_else(|| nonempty(&info.author_email));

    let url = nonempty(&info.package_url)
        .and_then(absolutize)
        .or_else(|| nonempty(&info.project_url).and_then(absolutize))
        .unwrap_or_else(|| format!("{HOST}/project/{name}/"));

    let title = match version {
        Some(version) => format!("{name} {version}"),
        None => name.to_string(),
    };

    let description = match nonempty(&info.summary) {
        Some(summary) => tidy(summary),
        None => synthesize_description(name, version, author),
    };

    Ok(vec![RawResult {
        url,
        title,
        description: truncate(&description, 300),
    }])
}

#[async_trait]
impl SearchEngine for PyPi {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // One name, one result: everything past the first page is exhausted.
        if page_number(start, PER_PAGE) > 1 {
            return Ok(Vec::new());
        }

        let Some(name) = candidate_name(query) else {
            return Ok(Vec::new());
        };

        let resp = browser_client()
            .get(build_lookup_url(&name))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        // A 404 is the API's way of saying "no package by that name", which
        // is an empty result set, not a failure. Checked before
        // `body_or_block` so it can't be mistaken for a block.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }

        let body = body_or_block(resp, "PyPI").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_lookup_url_targets_the_json_api_for_the_first_page() {
        assert_eq!(
            build_lookup_url(&candidate_name("pangolin").unwrap()),
            "https://pypi.org/pypi/pangolin/json"
        );
    }

    #[test]
    fn candidate_name_applies_pep_503_normalization() {
        assert_eq!(candidate_name("Django").unwrap(), "django");
        assert_eq!(
            candidate_name("python_dateutil").unwrap(),
            "python-dateutil"
        );
        assert_eq!(candidate_name("Zope.Interface").unwrap(), "zope-interface");
        // Repeated and mixed separators collapse to a single dash.
        assert_eq!(
            candidate_name("ruamel..yaml__clib").unwrap(),
            "ruamel-yaml-clib"
        );
        assert_eq!(
            candidate_name("  -typing-extensions-  ").unwrap(),
            "typing-extensions"
        );
    }

    /// A space is a separator, not something to escape: PyPI has no package
    /// names with spaces, so `python dateutil` should reach
    /// `python-dateutil` rather than 404 on `python%20dateutil`.
    #[test]
    fn build_lookup_url_folds_a_space_into_a_dash_and_escapes_non_ascii() {
        assert_eq!(
            build_lookup_url(&candidate_name("python dateutil").unwrap()),
            "https://pypi.org/pypi/python-dateutil/json"
        );
        assert_eq!(
            build_lookup_url(&candidate_name("café").unwrap()),
            "https://pypi.org/pypi/caf%C3%A9/json"
        );
    }

    /// `encode` is tuned for query strings and leaves `/` intact, so a query
    /// like `../../admin` would otherwise rewrite the request path.
    #[test]
    fn candidate_name_rejects_queries_that_would_escape_the_url_path() {
        assert_eq!(candidate_name("../../etc/passwd"), None);
        assert_eq!(candidate_name("requests?x=1"), None);
        assert_eq!(candidate_name("a%2fb"), None);
        assert_eq!(candidate_name("   "), None);
        assert_eq!(candidate_name("..."), None);
    }

    #[test]
    fn absolutize_turns_a_relative_project_path_into_an_absolute_pypi_url() {
        assert_eq!(
            absolutize("/project/pangolin/").unwrap(),
            "https://pypi.org/project/pangolin/"
        );
        assert_eq!(
            absolutize("http://pypi.org/project/pangolin/").unwrap(),
            "https://pypi.org/project/pangolin/"
        );
        assert_eq!(
            absolutize("https://pypi.org/project/pangolin/").unwrap(),
            "https://pypi.org/project/pangolin/"
        );
        assert_eq!(absolutize("javascript:alert(1)"), None);
    }

    #[test]
    fn parse_response_falls_back_to_a_constructed_project_url() {
        let body = r#"{"info":{"name":"pangolin","version":"0.0.6","summary":"Fun."}}"#;
        let results = parse_response(body).unwrap();
        assert_eq!(results[0].url, "https://pypi.org/project/pangolin/");
    }

    #[test]
    fn parse_response_synthesizes_a_description_when_the_summary_is_missing() {
        let body = r#"{"info":{"name":"lonely","version":"1.2.3","summary":null,
                       "author":"A. Hacker","package_url":"/project/lonely/"}}"#;
        let results = parse_response(body).unwrap();
        assert_eq!(
            results[0].description,
            "Version 1.2.3 of the Python package lonely, on PyPI, by A. Hacker."
        );
    }

    #[test]
    fn parse_response_treats_the_not_found_body_as_an_empty_result_set() {
        // PyPI's 404 body — well-formed JSON with no project in it.
        let results = parse_response(r#"{"message": "Not Found"}"#).unwrap();
        assert!(results.is_empty());
    }

    /// Trimmed from the real interstitial PyPI's `/search/` endpoint serves
    /// to non-browser clients (captured 2026-09-07), which arrives with
    /// HTTP 200 and so is invisible to the status-code classifier.
    const CHALLENGE_PAGE: &str = r#"<!DOCTYPE html>
        <html lang="en"><head><title>Client Challenge</title>
        <link href="/_fs-ch-1T1wmsGaOgGaSxcX/assets/styles.css" rel="stylesheet" />
        </head><body><noscript>Please enable JavaScript to proceed.</noscript></body></html>"#;

    #[test]
    fn looks_like_project_json_rejects_a_bot_challenge_page() {
        assert!(looks_like_project_json(r#"{"info":{}}"#));
        assert!(!looks_like_project_json(CHALLENGE_PAGE));
    }

    #[test]
    fn parse_response_errors_rather_than_reporting_no_package_on_a_challenge_page() {
        assert!(matches!(
            parse_response(CHALLENGE_PAGE),
            Err(EngineError::ParseError(_))
        ));
    }

    #[test]
    fn parse_response_extracts_the_project_from_the_real_fixture() {
        let results = parse_response(&fixture("pypi.json")).unwrap();

        assert_eq!(results.len(), 1, "an exact-name lookup yields one project");
        assert_eq!(results[0].url, "https://pypi.org/project/pangolin/");
        assert_eq!(results[0].title, "pangolin 0.0.6");
        assert_eq!(
            results[0].description,
            "Probabilistic inference focused on fun"
        );
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("pypi.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must carry an openable url, a title, and a description"
        );
    }

    /// Must short-circuit before any request, so this stays offline.
    #[tokio::test]
    async fn search_results_reports_exhaustion_past_the_first_page() {
        let results = PyPi.search_results("pangolin", PER_PAGE, 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_results_returns_nothing_for_a_query_that_is_not_a_package_name() {
        let results = PyPi
            .search_results("../../etc/passwd", 0, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn test_pypi_search_live() {
        let results = PyPi.search_results("requests", 0, 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://pypi.org/project/requests/");
        assert!(results[0].title.starts_with("requests "));
    }

    #[ignore]
    #[tokio::test]
    async fn test_pypi_unknown_package_is_empty_not_an_error_live() {
        let results = PyPi
            .search_results("this-package-surely-does-not-exist-xyz123", 0, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }
}
