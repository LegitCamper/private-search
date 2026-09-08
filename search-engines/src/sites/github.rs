//! GitHub repository search, via the public REST search API.
//!
//! Answers "which repo is called X" — for a programmer that is almost always
//! what a bare word like `ripgrep` or `pangolin` means, and it is exactly what
//! a general web engine is worst at (it returns blog posts *about* the repo).
//!
//! Quirks worth knowing before changing anything here:
//!
//! * **Unauthenticated search is ~10 requests/minute** (observed
//!   `x-ratelimit-limit: 10`, `x-ratelimit-resource: search`, reset in
//!   `x-ratelimit-reset`), an order of magnitude tighter than the 60/hour core
//!   limit. Exceeding it is a 403, which [`crate::body_or_block`] turns into
//!   `Blocked`/`AccessDenied` and the cooldown registry handles from there.
//! * **The result set is hard-capped at 1000 items.** Asking for page 51 at
//!   `per_page=20` is a *422*, not an empty page — and 422 is not a status
//!   `classify_status` treats as a block, so it would surface as a confusing
//!   parse error. We stop short of the cap instead ([`MAX_RESULTS`]).
//! * `description`, `language` and `pushed_at` are all nullable. Plenty of
//!   real repos have no description at all, so we synthesise one from the
//!   metadata rather than emit a blank row.
//! * The JSON carries literal text, not HTML — no entity decoding is needed
//!   (a `&` in a repo description arrives as `&`, verified against live
//!   responses). If that ever changes, decode before [`tidy`].
//!
//! `html_url` (not `url`, which is the API endpoint) is the human-facing page
//! and the only thing that belongs in a `RawResult`.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct GitHub;

impl EngineInfo for GitHub {
    fn name(&self) -> &'static str {
        "GitHub"
    }
}

/// GitHub's `per_page` maxes out at 100, but 20 keeps each request cheap
/// against a 10/minute budget and matches the batch size the UI pages by.
const PER_PAGE: usize = 20;

/// The search API refuses to serve past its 1000th hit. Anything at or beyond
/// this offset is exhaustion, not an error.
const MAX_RESULTS: usize = 1000;

/// How many topics to list; a popular repo can carry twenty, which would eat
/// the whole description budget.
const MAX_TOPICS: usize = 5;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page={PER_PAGE}&page={page}",
        encode(query)
    )
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<Repository>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    html_url: String,
    full_name: String,
    description: Option<String>,
    language: Option<String>,
    stargazers_count: u64,
    #[serde(default)]
    topics: Vec<String>,
    pushed_at: Option<String>,
}

/// `11814` -> `11,814`. Star counts are the main signal in a repo result and
/// unseparated six-digit numbers are genuinely hard to compare at a glance.
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

/// Builds the result blurb: the repo's own description plus the metadata that
/// actually decides whether a hit is worth clicking, or — when the repo has no
/// description, which is common — that metadata standing on its own.
fn describe(repo: &Repository) -> String {
    let mut facts = vec![format!("★ {}", thousands(repo.stargazers_count))];

    if let Some(language) = repo.language.as_deref().map(str::trim)
        && !language.is_empty()
    {
        facts.push(language.to_string());
    }

    let topics: Vec<&str> = repo
        .topics
        .iter()
        .map(String::as_str)
        .filter(|t| !t.trim().is_empty())
        .take(MAX_TOPICS)
        .collect();
    if !topics.is_empty() {
        facts.push(topics.join(", "));
    }

    let blurb = repo
        .description
        .as_deref()
        .map(tidy)
        .filter(|d| !d.is_empty());

    let text = match blurb {
        Some(blurb) => format!("{blurb} · {}", facts.join(" · ")),
        None => {
            // With no prose to lead with, "is this still alive?" is the most
            // useful thing left, so spend the extra space on the push date.
            if let Some(date) = repo.pushed_at.as_deref().and_then(|t| t.get(..10)) {
                facts.push(format!("last pushed {date}"));
            }
            format!("{} · {}", repo.full_name, facts.join(" · "))
        }
    };

    truncate(&tidy(&text), 300)
}

fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "GitHub returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(response
        .items
        .iter()
        // A repo with no `html_url` isn't openable, and `full_name` is the
        // title — an entry missing either would violate the non-empty
        // contract, so drop it rather than ship a broken row.
        .filter(|repo| repo.html_url.starts_with("https://") && !repo.full_name.trim().is_empty())
        .map(|repo| RawResult {
            url: repo.html_url.clone(),
            title: repo.full_name.clone(),
            description: describe(repo),
        })
        .collect())
}

#[async_trait]
impl SearchEngine for GitHub {
    /// `count` is ignored: the page size is fixed at [`PER_PAGE`] so that a
    /// given `start` always lands on the same page boundary, which is what
    /// makes the cache layer's dedupe across pages work.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if start >= MAX_RESULTS {
            return Ok(Vec::new());
        }

        let body = fetch(&build_search_url(query, start)).await?;
        parse_response(&body)
    }
}

/// Fetches the search body as text.
///
/// Deliberately not `super::get_json_with`: that helper deserialises for you,
/// which would either duplicate the response shape or force a re-serialise
/// round trip. Going through [`crate::body_or_block`] directly keeps the
/// 403/429/5xx-to-`Blocked` classification while leaving [`parse_response`]
/// the one and only place that knows the JSON shape — so the unit tests
/// exercise exactly the code the live path runs.
async fn fetch(url: &str) -> Result<String, EngineError> {
    let resp = crate::browser_client()
        .get(url)
        // `accept` selects the response version; the API-version pin means a
        // future breaking release doesn't silently reshape our results.
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .send()
        .await
        .map_err(EngineError::ReqwestError)?;

    crate::body_or_block(resp, "GitHub").await
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_the_first_page_for_a_zero_start() {
        assert_eq!(
            build_search_url("rust async", 0),
            "https://api.github.com/search/repositories?q=rust%20async\
             &sort=stars&order=desc&per_page=20&page=1"
        );
    }

    #[test]
    fn build_search_url_advances_to_the_page_holding_start() {
        assert_eq!(
            build_search_url("rust async", PER_PAGE),
            "https://api.github.com/search/repositories?q=rust%20async\
             &sort=stars&order=desc&per_page=20&page=2"
        );
        assert_eq!(
            build_search_url("rust async", PER_PAGE * 4 + 7),
            "https://api.github.com/search/repositories?q=rust%20async\
             &sort=stars&order=desc&per_page=20&page=5"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii() {
        assert_eq!(
            build_search_url("café 日本語", 0),
            "https://api.github.com/search/repositories\
             ?q=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E\
             &sort=stars&order=desc&per_page=20&page=1"
        );
    }

    /// GitHub's own qualifier syntax has to survive encoding or the query
    /// silently means something else.
    #[test]
    fn build_search_url_keeps_qualifier_syntax_but_neutralises_an_ampersand() {
        assert_eq!(
            build_search_url("tui stars:>500 &foo", 0),
            "https://api.github.com/search/repositories\
             ?q=tui%20stars:%3E500%20%26foo\
             &sort=stars&order=desc&per_page=20&page=1"
        );
    }

    #[test]
    fn parse_response_returns_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("github.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("github.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be openable and labelled"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://github.com/")),
            "results must link to the human-facing page, never api.github.com"
        );
    }

    // Pinned against the response recorded on 2026-09-07 for
    // `q=rust http client`, so an upstream field rename fails loudly here
    // instead of quietly returning zero results to users.
    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("github.json")).unwrap();
        assert_eq!(results[0].url, "https://github.com/seanmonstar/reqwest");
        assert_eq!(results[0].title, "seanmonstar/reqwest");
        assert!(
            results[0]
                .description
                .starts_with("An easy and powerful Rust HTTP Client · ★ "),
            "unexpected description: {}",
            results[0].description
        );
        assert!(
            results[0].description.contains("· Rust ·"),
            "the language should be part of the blurb: {}",
            results[0].description
        );
    }

    /// Paging past the last hit yields a well-formed body with an empty
    /// `items` array; the cache layer reads that as exhaustion, so it must
    /// not become an error.
    #[test]
    fn parse_response_treats_an_empty_item_list_as_exhaustion_not_an_error() {
        let json = r#"{"total_count":0,"incomplete_results":false,"items":[]}"#;
        assert!(parse_response(json).unwrap().is_empty());
    }

    #[test]
    fn parse_response_reports_a_changed_api_shape_as_a_parse_error() {
        assert!(matches!(
            parse_response(r#"{"message":"Only the first 1000 search results are available"}"#),
            Err(EngineError::ParseError(_))
        ));
    }

    // Hand-written rather than taken from the fixture: top-starred repos all
    // have descriptions, but plenty of real hits deeper in the result set
    // have `description: null`, and that row still has to be non-empty.
    #[test]
    fn describe_synthesises_a_blurb_when_the_repo_has_no_description() {
        let json = r#"{"items":[{
            "html_url": "https://github.com/octocat/bare",
            "full_name": "octocat/bare",
            "description": null,
            "language": null,
            "stargazers_count": 1234567,
            "topics": [],
            "pushed_at": "2026-09-07T12:37:52Z"
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].description,
            "octocat/bare · ★ 1,234,567 · last pushed 2026-09-07"
        );
    }

    #[test]
    fn describe_caps_the_topic_list_and_the_overall_length() {
        let json = r#"{"items":[{
            "html_url": "https://github.com/octocat/loud",
            "full_name": "octocat/loud",
            "description": "one   two\n\nthree",
            "language": "Rust",
            "stargazers_count": 7,
            "topics": ["a","b","c","d","e","f","g"],
            "pushed_at": "2026-09-07T12:37:52Z"
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].description,
            "one two three · ★ 7 · Rust · a, b, c, d, e"
        );
        assert!(results[0].description.chars().count() <= 300);
    }

    #[test]
    fn parse_response_drops_an_entry_with_no_openable_url() {
        let json = r#"{"items":[
            {"html_url":"","full_name":"octocat/nowhere","description":"x",
             "language":null,"stargazers_count":0,"topics":[],"pushed_at":null},
            {"html_url":"https://github.com/octocat/real","full_name":"octocat/real",
             "description":"x","language":null,"stargazers_count":0,"topics":[],
             "pushed_at":null}
        ]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://github.com/octocat/real");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(11814), "11,814");
    }

    #[tokio::test]
    async fn search_results_stops_at_the_thousand_result_cap_without_a_request() {
        // Page 51 is a 422 upstream, so this must short-circuit rather than
        // hit the network — no `#[ignore]`, precisely because it makes no
        // request.
        let results = GitHub
            .search_results("rust", MAX_RESULTS, PER_PAGE)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn github_search_live() {
        let results = GitHub
            .search_results("rust http client", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://github.com/") && !r.description.is_empty())
        );
    }

    #[ignore]
    #[tokio::test]
    async fn github_search_pagination_live() {
        let page1 = GitHub
            .search_results("rust http client", 0, 20)
            .await
            .unwrap();
        let page2 = GitHub
            .search_results("rust http client", PER_PAGE, 20)
            .await
            .unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "page 2 should advance, not repeat page 1"
        );
    }
}
