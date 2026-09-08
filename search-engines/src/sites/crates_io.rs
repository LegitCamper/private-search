//! crates.io — the Rust package registry.
//!
//! Answers "which crate does what I mean", which is what a Rust programmer
//! searching for `async runtime` or `serde yaml` actually wants: the crate
//! itself, not a blog post about it. Results link to the human crate page
//! and carry the version, download count and docs.rs link in the
//! description, because those are what you check before adding a dependency.
//!
//! Quirks worth knowing:
//!
//! * **User-Agent.** crates.io's crawler policy requires a User-Agent that
//!   identifies the client and gives the operator a way to make contact;
//!   requests with no UA are refused outright (verified: HTTP 403). The
//!   shared [`browser_client`](crate::browser_client) sends a generic
//!   browser UA, which is exactly the kind of anonymous traffic that policy
//!   exists to stop, so every request here overrides it with a project
//!   identifier plus repository URL.
//! * **Hard paging cap.** The API rejects any request where
//!   `page * per_page > 1000` with an HTTP 400 ("Page N is unavailable for
//!   performance reasons"), regardless of how large `meta.total` is. That
//!   400 is *not* a block, so it would surface as a confusing parse error;
//!   we stop paging before reaching it instead.
//! * **`meta.total`.** Past the last page crates.io returns
//!   `{"crates":[],"meta":{"total":0,…}}` — an empty page rather than an
//!   error — which is the exhaustion signal the cache layer wants.
//! * Several fields are nullable: `description`, `repository`, `homepage`
//!   and `documentation` can all be `null`, and `keywords`/`categories` are
//!   *always* `null` in search results (they're only populated on the
//!   single-crate endpoint), so a description can't be synthesised from
//!   them.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json_with, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct CratesIo;

impl EngineInfo for CratesIo {
    fn name(&self) -> &'static str {
        "crates.io"
    }
}

/// crates.io's `per_page` maximum is 100, but 20 keeps a page small and
/// matches the batch size the rest of the pipeline pages by.
const PER_PAGE: usize = 20;

/// crates.io refuses `page * per_page > 1000`, so page 50 is the last one it
/// will serve at [`PER_PAGE`] results each.
const MAX_RESULTS: usize = 1000;

/// Identifies the client and points at the project, per crates.io's crawler
/// policy <https://crates.io/policies#crawlers>. Keep the contact URL in it.
const USER_AGENT: &str = "private-search (+https://github.com/legitcamper/private-search)";

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://crates.io/api/v1/crates?q={}&page={page}&per_page={PER_PAGE}&sort=relevance",
        encode(query)
    )
}

/// The subset of crates.io's search response we use. Everything here that
/// the API declares nullable is an `Option`; unknown fields are ignored, so
/// new ones upstream can't break deserialization.
#[derive(Deserialize)]
struct SearchResponse {
    crates: Vec<Crate>,
}

#[derive(Deserialize)]
struct Crate {
    name: String,
    description: Option<String>,
    max_version: Option<String>,
    #[serde(default)]
    downloads: u64,
    repository: Option<String>,
}

/// `12475` → `"12,475"`. Raw download counts run to eight digits and are
/// unreadable without separators, and they're the single strongest "is this
/// crate actually used" signal in a result row.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Builds the result blurb: the crate's own description (when it has one)
/// followed by the facts you'd check next — version, downloads, and the
/// docs.rs link, which is almost always where a Rust programmer goes after
/// finding a crate.
fn describe(krate: &Crate) -> String {
    let version = krate
        .max_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown version");

    // Truncated before the metadata is appended so a long upstream blurb
    // can't push the version and docs link past the 300-char cut.
    let blurb = truncate(&tidy(krate.description.as_deref().unwrap_or_default()), 170);

    let mut description = if blurb.is_empty() {
        // A crate with no description still needs a non-empty one; the
        // registry facts are all we have, since search results never carry
        // keywords or categories.
        format!(
            "Rust crate — v{version}, {} downloads",
            thousands(krate.downloads)
        )
    } else {
        format!(
            "{blurb} — v{version}, {} downloads",
            thousands(krate.downloads)
        )
    };

    description.push_str(&format!(" — docs at https://docs.rs/{}", krate.name));

    if let Some(repository) = krate
        .repository
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        description.push_str(&format!(" — repo: {repository}"));
    }

    truncate(&tidy(&description), 300)
}

/// Pure parse step, so tests can drive it from the recorded fixture.
///
/// An empty `crates` array parses to an empty `Vec`, not an error: that's
/// how crates.io reports both "no matches" and "you're past the last page",
/// and the cache layer reads an empty vec as exhaustion.
#[cfg(test)]
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "crates.io returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(to_results(&response))
}

fn to_results(response: &SearchResponse) -> Vec<RawResult> {
    response
        .crates
        .iter()
        // A nameless crate can't be linked or titled; the registry never
        // serves one, but skipping keeps the "all fields non-empty"
        // guarantee true by construction rather than by trust.
        .filter(|krate| !krate.name.trim().is_empty())
        .map(|krate| RawResult {
            url: format!("https://crates.io/crates/{}", krate.name),
            title: krate.name.clone(),
            description: describe(krate),
        })
        .collect()
}

#[async_trait]
impl SearchEngine for CratesIo {
    /// `count` is ignored: the page size is fixed at [`PER_PAGE`] so that
    /// `start` maps cleanly onto crates.io's 1-based `page`.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // Asking for a page past the registry's cap earns an HTTP 400 with
        // an error body, which would deserialize-fail and look like a broken
        // parser. Treat the cap as exhaustion, which it effectively is.
        if start >= MAX_RESULTS {
            return Ok(Vec::new());
        }

        let response: SearchResponse = get_json_with(
            &build_search_url(query, start),
            "crates.io",
            &[("user-agent", USER_AGENT)],
        )
        .await?;

        Ok(to_results(&response))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_asks_for_the_first_page_when_start_is_zero() {
        assert_eq!(
            build_search_url("async runtime", 0),
            "https://crates.io/api/v1/crates?q=async%20runtime&page=1&per_page=20&sort=relevance"
        );
    }

    #[test]
    fn build_search_url_advances_to_the_page_holding_start() {
        assert_eq!(
            build_search_url("async runtime", PER_PAGE),
            "https://crates.io/api/v1/crates?q=async%20runtime&page=2&per_page=20&sort=relevance"
        );
        assert_eq!(
            build_search_url("async runtime", PER_PAGE * 4 + 7),
            "https://crates.io/api/v1/crates?q=async%20runtime&page=5&per_page=20&sort=relevance"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_in_the_query() {
        assert_eq!(
            build_search_url("café parser", 0),
            "https://crates.io/api/v1/crates?q=caf%C3%A9%20parser&page=1&per_page=20&sort=relevance"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_from_the_recorded_fixture() {
        let results = parse_response(&fixture("crates_io.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_recorded_fixture() {
        let results = parse_response(&fixture("crates_io.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be openable and readable by a human"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://crates.io/crates/")),
            "results must link the human crate page, never the API"
        );
    }

    /// Pinned to the real response recorded on 2026-09-07, so a field rename
    /// upstream fails loudly here instead of silently emptying results.
    #[test]
    fn parse_response_first_result_matches_the_recorded_response() {
        let results = parse_response(&fixture("crates_io.json")).unwrap();
        assert_eq!(
            results[0].url,
            "https://crates.io/crates/pyo3-async-runtimes-macros"
        );
        assert_eq!(results[0].title, "pyo3-async-runtimes-macros");
    }

    #[test]
    fn parse_response_puts_version_downloads_and_docs_link_in_the_description() {
        let results = parse_response(&fixture("crates_io.json")).unwrap();
        assert!(results[0].description.contains("v0.29.0"));
        assert!(results[0].description.contains("11,463,090 downloads"));
        assert!(
            results[0]
                .description
                .contains("https://docs.rs/pyo3-async-runtimes-macros")
        );
    }

    /// crates.io's schema makes `description` nullable, but the live API
    /// served no null-description crate across ~1500 sampled results, so
    /// this case is exercised from a hand-written body rather than the
    /// recorded fixture.
    const NULL_DESCRIPTION_RESPONSE: &str = r#"{
        "crates": [
            {
                "id": "quiet-crate",
                "name": "quiet-crate",
                "description": null,
                "max_version": "1.4.2",
                "downloads": 4210,
                "repository": null,
                "keywords": null,
                "categories": null
            }
        ],
        "meta": {"total": 1, "next_page": null, "prev_page": null}
    }"#;

    #[test]
    fn parse_response_synthesises_a_description_for_a_crate_that_has_none() {
        let results = parse_response(NULL_DESCRIPTION_RESPONSE).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].description,
            "Rust crate — v1.4.2, 4,210 downloads — docs at https://docs.rs/quiet-crate"
        );
    }

    /// Past the last page crates.io answers with an empty `crates` array;
    /// that has to read as exhaustion, not as a broken response.
    #[test]
    fn parse_response_treats_a_well_formed_empty_page_as_no_more_results() {
        let empty = r#"{"crates":[],"meta":{"total":0,"next_page":null,"prev_page":null}}"#;
        assert_eq!(parse_response(empty).unwrap(), Vec::new());
    }

    #[test]
    fn parse_response_reports_an_unexpected_body_as_a_parse_error() {
        // crates.io answers a too-deep page with `{"errors":[…]}`.
        let errors = r#"{"errors":[{"detail":"Page 500 is unavailable"}]}"#;
        assert!(matches!(
            parse_response(errors),
            Err(EngineError::ParseError(_))
        ));
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(11_463_090), "11,463,090");
    }

    #[tokio::test]
    async fn search_results_stops_at_the_registry_paging_cap_without_a_request() {
        let results = CratesIo.search_results("serde", MAX_RESULTS, 20).await;
        assert_eq!(results.unwrap(), Vec::new());
    }

    #[ignore]
    #[tokio::test]
    async fn test_crates_io_search_live() {
        let results = CratesIo
            .search_results("async runtime", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty())
        );
    }

    #[ignore]
    #[tokio::test]
    async fn test_crates_io_pagination_live() {
        let page1 = CratesIo
            .search_results("async runtime", 0, 20)
            .await
            .unwrap();
        let page2 = CratesIo
            .search_results("async runtime", PER_PAGE, 20)
            .await
            .unwrap();

        assert!(!page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "page 2 should advance, not repeat page 1"
        );
    }
}
