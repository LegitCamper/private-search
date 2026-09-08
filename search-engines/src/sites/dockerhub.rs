//! Docker Hub — searches the container image registry, so a query for
//! `postgres` surfaces the *image you can pull* rather than a blog post about
//! Postgres in Docker.
//!
//! ## Endpoint
//!
//! Uses the long-standing `https://hub.docker.com/v2/search/repositories/`.
//! It answers HTTP 200 to anonymous requests with no headers beyond a normal
//! browser User-Agent, and returns exactly the four fields this engine needs
//! (`repo_name`, `short_description`, `star_count`, `pull_count`) as plain
//! scalars.
//!
//! The two newer endpoints were tried and rejected:
//!
//! * `api/content/v1/products/search` (with `Search-Version: v3`) answers 200
//!   with a literal `{}` — no results at all — so it is simply dead.
//! * `api/search/v3/catalog/search` does work, but its payload is the shape
//!   that backs the website's faceted UI: results nest their real repositories
//!   under a `rate_plans[].repositories[]` array, `pull_count` arrives
//!   pre-formatted as a *string* (`"5M"`) rather than a number, and
//!   `star_count` reads 0 for Docker-Hardened-Image entries. It is strictly
//!   more work to parse for strictly less signal.
//!
//! ## Quirks
//!
//! * **The human-facing URL is not the API name.** Official ("library")
//!   images have no `/` in `repo_name` and live at `hub.docker.com/_/<name>`;
//!   everything else lives at `hub.docker.com/r/<namespace>/<name>`. Getting
//!   this backwards yields a 404 for the most popular images on the site.
//! * **Anonymous paging stops at 200 results.** `page * page_size > 200`
//!   returns HTTP 403 `"pagination too large for anonymous requests"` — not
//!   404, not an empty page. Since [`crate::body_or_block`] would (correctly)
//!   read that 403 as a block and put the whole engine into cooldown, this
//!   engine refuses to build the request at all past the ceiling and reports
//!   exhaustion instead.
//! * Anonymous traffic is rate limited by IP at 180 requests per window
//!   (`x-ratelimit-limit: 180`, with `x-ratelimit-remaining` / `-reset`
//!   counting down); exceeding it is a 429, which the block classifier
//!   already handles.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client};

#[derive(Clone)]
pub struct DockerHub;

impl EngineInfo for DockerHub {
    fn name(&self) -> &'static str {
        "Docker Hub"
    }
}

/// Fixed by us, not by the API — `page_size` is free-form, but the anonymous
/// result ceiling is an offset (`page * page_size`), so a bigger page buys
/// nothing but a coarser cursor.
const PER_PAGE: usize = 20;

/// Anonymous requests may not reach an offset beyond this; see the module
/// docs. Ten pages of [`PER_PAGE`].
const MAX_ANONYMOUS_RESULTS: usize = 200;

/// The subset of `/v2/search/repositories/` we consume. Extra fields on the
/// wire (`repo_owner`, `is_automated`, `count`, `next`, …) are ignored.
#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<Repository>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    /// `"nginx"` for an official image, `"linuxserver/nginx"` otherwise.
    repo_name: String,
    /// Present but frequently the empty string.
    #[serde(default)]
    short_description: String,
    #[serde(default)]
    star_count: u64,
    #[serde(default)]
    pull_count: u64,
    #[serde(default)]
    is_official: bool,
}

/// `None` once `start` has run past what Docker Hub will page to anonymously,
/// which the caller turns into "no more results" rather than a doomed request.
fn build_search_url(query: &str, start: usize) -> Option<String> {
    if start >= MAX_ANONYMOUS_RESULTS {
        return None;
    }
    let page = page_number(start, PER_PAGE);
    Some(format!(
        "https://hub.docker.com/v2/search/repositories/?query={}&page={page}&page_size={PER_PAGE}",
        encode(query)
    ))
}

/// Maps an API `repo_name` to the page a human can actually open. Official
/// images sit in the implicit `library` namespace and are served from `/_/`;
/// a namespaced repo is served from `/r/`.
fn repo_page_url(repo_name: &str) -> String {
    if repo_name.contains('/') {
        format!("https://hub.docker.com/r/{repo_name}")
    } else {
        format!("https://hub.docker.com/_/{repo_name}")
    }
}

/// `13334094247` → `"13.3B"`. Raw pull counts run to eleven digits, which
/// reads as noise in a one-line result row.
fn compact_count(n: u64) -> String {
    const UNITS: [(u64, char); 3] = [(1_000_000_000, 'B'), (1_000_000, 'M'), (1_000, 'K')];
    for (scale, suffix) in UNITS {
        if n >= scale {
            // One decimal, but not a pointless ".0".
            let tenths = n * 10 / scale;
            let (whole, frac) = (tenths / 10, tenths % 10);
            return if frac == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{frac}{suffix}")
            };
        }
    }
    n.to_string()
}

/// Stars and pulls *are* the relevance signal for an image — an image with
/// 21k stars and 13B pulls is the one you meant — so they lead the
/// description rather than trailing it. They also guarantee a non-empty
/// description for the many repos that publish no `short_description`.
fn describe(repo: &Repository) -> String {
    let mut parts = vec![
        format!("★ {}", compact_count(repo.star_count)),
        format!("{} pulls", compact_count(repo.pull_count)),
    ];
    if repo.is_official {
        parts.push("official image".to_string());
    }
    let mut description = parts.join(" · ");

    let summary = tidy(&repo.short_description);
    if !summary.is_empty() {
        description.push_str(" — ");
        description.push_str(&summary);
    }

    truncate(&tidy(&description), 300)
}

pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "Docker Hub returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(response
        .results
        .iter()
        // A repo with no name has no openable page; it would surface as a
        // result with an empty url and title.
        .filter(|repo| !repo.repo_name.trim().is_empty())
        .map(|repo| RawResult {
            url: repo_page_url(repo.repo_name.trim()),
            title: repo.repo_name.trim().to_string(),
            description: describe(repo),
        })
        .collect())
}

#[async_trait]
impl SearchEngine for DockerHub {
    /// `count` is ignored: the page size is ours to choose and changing it
    /// per call would misalign `start` with the pages already fetched.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let Some(url) = build_search_url(query, start) else {
            return Ok(Vec::new());
        };

        // Fetched as text rather than via `get_json` so `parse_response` stays
        // a pure function the tests can drive off the recorded fixture.
        let resp = browser_client()
            .get(&url)
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;
        let body = body_or_block(resp, "Docker Hub").await?;

        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_page_one_for_a_fresh_query() {
        assert_eq!(
            build_search_url("nginx", 0).unwrap(),
            "https://hub.docker.com/v2/search/repositories/?query=nginx&page=1&page_size=20"
        );
    }

    #[test]
    fn build_search_url_advances_to_the_page_holding_start() {
        assert_eq!(
            build_search_url("nginx", PER_PAGE).unwrap(),
            "https://hub.docker.com/v2/search/repositories/?query=nginx&page=2&page_size=20"
        );
        assert_eq!(
            build_search_url("nginx", PER_PAGE * 3 + 7).unwrap(),
            "https://hub.docker.com/v2/search/repositories/?query=nginx&page=4&page_size=20"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_queries() {
        assert_eq!(
            build_search_url("café serveur", 0).unwrap(),
            "https://hub.docker.com/v2/search/repositories/\
             ?query=caf%C3%A9%20serveur&page=1&page_size=20"
        );
    }

    /// Past the anonymous ceiling Docker Hub answers 403, which the block
    /// classifier would read as a ban and cool the whole engine down. Refuse
    /// to ask instead.
    #[test]
    fn build_search_url_refuses_to_page_past_the_anonymous_ceiling() {
        assert!(build_search_url("nginx", MAX_ANONYMOUS_RESULTS - PER_PAGE).is_some());
        assert!(build_search_url("nginx", MAX_ANONYMOUS_RESULTS).is_none());
        assert!(build_search_url("nginx", MAX_ANONYMOUS_RESULTS * 5).is_none());
    }

    #[test]
    fn repo_page_url_sends_official_images_to_the_library_path() {
        assert_eq!(repo_page_url("nginx"), "https://hub.docker.com/_/nginx");
        assert_eq!(
            repo_page_url("postgres"),
            "https://hub.docker.com/_/postgres"
        );
    }

    #[test]
    fn repo_page_url_sends_namespaced_images_to_the_repository_path() {
        assert_eq!(
            repo_page_url("linuxserver/nginx"),
            "https://hub.docker.com/r/linuxserver/nginx"
        );
    }

    #[test]
    fn compact_count_abbreviates_large_counts_and_leaves_small_ones_alone() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(16_206), "16.2K");
        assert_eq!(compact_count(2_000_000), "2M");
        assert_eq!(compact_count(13_334_094_247), "13.3B");
    }

    #[test]
    fn parse_response_reads_every_repository_in_the_real_fixture() {
        let results = parse_response(&fixture("dockerhub.json")).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_in_the_real_fixture() {
        let results = parse_response(&fixture("dockerhub.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "several fixture repos publish no short_description, so the \
             star/pull summary has to stand in for it"
        );
        assert!(
            results.iter().all(|r| r.url.starts_with("https://")),
            "urls must be absolute pages a human can open"
        );
    }

    #[test]
    fn parse_response_matches_the_recorded_first_result() {
        let results = parse_response(&fixture("dockerhub.json")).unwrap();
        assert_eq!(results[0].url, "https://hub.docker.com/_/nginx");
        assert_eq!(results[0].title, "nginx");
        assert_eq!(
            results[0].description,
            "★ 21.3K · 13.3B pulls · official image — Official build of Nginx."
        );
    }

    /// The fixture's first hit is the official `nginx` and its second is the
    /// namespaced `nginx/nginx-ingress`, so one real page exercises both
    /// halves of the URL branch.
    #[test]
    fn parse_response_branches_the_url_by_namespace_on_the_real_fixture() {
        let results = parse_response(&fixture("dockerhub.json")).unwrap();
        assert_eq!(
            results[1].url,
            "https://hub.docker.com/r/nginx/nginx-ingress"
        );
    }

    /// A query with no hits is exhaustion, not a failure — the cache layer
    /// reads the empty vec as "stop asking this engine".
    #[test]
    fn parse_response_treats_a_well_formed_empty_result_set_as_no_results() {
        let empty = r#"{"count":0,"next":"","previous":"","results":[]}"#;
        assert_eq!(parse_response(empty).unwrap().len(), 0);
    }

    #[test]
    fn parse_response_reports_a_body_that_is_not_the_expected_shape() {
        assert!(parse_response(r#"{"message":"forbidden"}"#).is_err());
        assert!(parse_response("<html>nope</html>").is_err());
    }

    #[ignore]
    #[tokio::test]
    async fn test_dockerhub_search_live() {
        let results = DockerHub.search_results("nginx", 0, 20).await.unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .any(|r| r.url == "https://hub.docker.com/_/nginx")
        );
    }

    #[ignore]
    #[tokio::test]
    async fn test_dockerhub_pagination_live() {
        let page1 = DockerHub.search_results("nginx", 0, 20).await.unwrap();
        let page2 = DockerHub
            .search_results("nginx", PER_PAGE, 20)
            .await
            .unwrap();

        assert!(!page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "pagination should advance, not repeat page 1"
        );
    }
}
