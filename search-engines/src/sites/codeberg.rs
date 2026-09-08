//! Codeberg repository search, via the public Forgejo/Gitea REST API.
//!
//! Codeberg is the main non-GitHub FOSS forge, so it answers the same "which
//! repo is called X" question [`super::github`] does, for the half of the
//! ecosystem that has left GitHub. No token, no account, no auth header.
//!
//! Quirks worth knowing before changing anything here:
//!
//! * **`q` alone matches the repo *name* only.** Searching `rust http client`
//!   without `includeDesc` returns literally zero hits, because no repository
//!   is *named* that. `includeDesc=true` widens the match to the description
//!   and is what makes multi-word queries — i.e. most real queries — work at
//!   all. It is not the default; do not drop it.
//! * **`limit` is clamped server-side at 50** (Forgejo's `MAX_RESPONSE_ITEMS`);
//!   asking for 100 silently yields 50. [`PER_PAGE`] stays well under that.
//! * **Success and failure are different shapes.** A 200 is
//!   `{"ok":true,"data":[…]}`; a rejected request is `{"message":…,"url":…}`
//!   with a 4xx — a bad `sort` value is a *422*, which
//!   [`crate::classify_status`] does not treat as a block, so it would arrive
//!   here as a body with no `ok` field. `ok` is therefore a required field:
//!   an error body fails to deserialise and surfaces as a loud
//!   [`EngineError::ParseError`] instead of a silent empty page.
//! * **Paging past the end is not an error.** Page 200 of a 40-hit query
//!   returns `{"ok":true,"data":[]}`, so there is no offset cap to guard
//!   against — exhaustion arrives as an empty `data` array and the cache layer
//!   reads that as "no more results".
//! * `description` and `language` are empty *strings*, never null, and a repo
//!   with no topics gets `[]` rather than null. They are still modelled as
//!   optional so a future Forgejo release that starts sending null doesn't
//!   break every result. Empty descriptions are extremely common (49 of 50 in
//!   one sampled page), so the blurb is synthesised from metadata.
//! * `html_url` is the human-facing page; `url` is the API endpoint and must
//!   never reach a `RawResult`.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_json, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Codeberg;

impl EngineInfo for Codeberg {
    fn name(&self) -> &'static str {
        "Codeberg"
    }
}

/// Results per request. Forgejo clamps `limit` at 50; 20 matches the batch
/// size the rest of the pipeline pages by, so a given `start` always lands on
/// the same page boundary.
const PER_PAGE: usize = 20;

/// How many topics to list. A repo can carry a dozen, which would crowd out
/// the description itself.
const MAX_TOPICS: usize = 5;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://codeberg.org/api/v1/repos/search?q={}&includeDesc=true\
         &limit={PER_PAGE}&page={page}&sort=stars&order=desc",
        encode(query)
    )
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    /// Required on purpose — see the module docs. An error body has no `ok`,
    /// so leaving it non-optional turns "the API refused us" into a parse
    /// error rather than a page that looks empty.
    ok: bool,
    /// Absent on an `ok: false` body, which we still need to deserialise far
    /// enough to read the message out of.
    #[serde(default)]
    data: Vec<Repository>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    html_url: String,
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    stars_count: u64,
    #[serde(default)]
    forks_count: u64,
    #[serde(default)]
    topics: Option<Vec<String>>,
    #[serde(default)]
    updated_at: Option<String>,
}

/// `11814` -> `11,814`. Star counts are the main signal in a repo result and
/// unseparated six-digit numbers are hard to compare at a glance.
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
/// decides whether a hit is worth clicking, or — when the repo has no
/// description, which on Codeberg is the common case — that metadata standing
/// on its own.
fn describe(repo: &Repository) -> String {
    let mut facts = vec![format!("★ {}", thousands(repo.stars_count))];

    if repo.forks_count > 0 {
        facts.push(format!("⑂ {}", thousands(repo.forks_count)));
    }

    if let Some(language) = repo.language.as_deref().map(str::trim)
        && !language.is_empty()
    {
        facts.push(language.to_string());
    }

    let topics: Vec<&str> = repo
        .topics
        .iter()
        .flatten()
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
            // useful thing left, so spend the extra space on the update date.
            if let Some(date) = repo.updated_at.as_deref().and_then(|t| t.get(..10)) {
                facts.push(format!("updated {date}"));
            }
            format!("{} · {}", repo.full_name, facts.join(" · "))
        }
    };

    truncate(&tidy(&text), 300)
}

fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "Codeberg returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    if !response.ok {
        let detail = response
            .error
            .or(response.message)
            .unwrap_or_else(|| "no message given".to_string());
        return Err(EngineError::ParseError(format!(
            "Codeberg reported a failed search: {detail}"
        )));
    }

    Ok(response
        .data
        .iter()
        // A repo with no `html_url` isn't openable and `full_name` is the
        // title, so an entry missing either would violate the non-empty
        // contract; drop it rather than ship a broken row.
        .filter(|repo| repo.html_url.starts_with("https://") && !repo.full_name.trim().is_empty())
        .map(|repo| RawResult {
            url: repo.html_url.clone(),
            title: repo.full_name.clone(),
            description: describe(repo),
        })
        .collect())
}

#[async_trait]
impl SearchEngine for Codeberg {
    /// `count` is ignored: the page size is fixed at [`PER_PAGE`] so that a
    /// given `start` always lands on the same page boundary, which is what
    /// makes the cache layer's dedupe across pages work.
    ///
    /// There is no offset cap to short-circuit — Codeberg answers a page past
    /// the end with an empty `data` array, which becomes the empty `Vec` the
    /// caller reads as exhaustion.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // Deserialising to `serde_json::Value` and re-serialising keeps the
        // 4xx/5xx-to-`Blocked` classification `get_json` provides while
        // leaving `parse_response` the single place that knows the response
        // shape, so the unit tests exercise exactly the live code path.
        let body: serde_json::Value = get_json(&build_search_url(query, start), "Codeberg").await?;
        parse_response(&body.to_string())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_the_first_page_for_a_zero_start() {
        assert_eq!(
            build_search_url("http client", 0),
            "https://codeberg.org/api/v1/repos/search?q=http%20client&includeDesc=true\
             &limit=20&page=1&sort=stars&order=desc"
        );
    }

    #[test]
    fn build_search_url_advances_to_the_page_holding_start() {
        assert_eq!(
            build_search_url("http client", PER_PAGE),
            "https://codeberg.org/api/v1/repos/search?q=http%20client&includeDesc=true\
             &limit=20&page=2&sort=stars&order=desc"
        );
        assert_eq!(
            build_search_url("http client", PER_PAGE * 4 + 7),
            "https://codeberg.org/api/v1/repos/search?q=http%20client&includeDesc=true\
             &limit=20&page=5&sort=stars&order=desc"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii() {
        assert_eq!(
            build_search_url("café 日本語", 0),
            "https://codeberg.org/api/v1/repos/search\
             ?q=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E&includeDesc=true\
             &limit=20&page=1&sort=stars&order=desc"
        );
    }

    /// An unescaped `&` would smuggle in a second query parameter and could
    /// override `sort` or `page`.
    #[test]
    fn build_search_url_neutralises_an_ampersand_in_the_query() {
        assert_eq!(
            build_search_url("a&page=99", 0),
            "https://codeberg.org/api/v1/repos/search?q=a%26page=99&includeDesc=true\
             &limit=20&page=1&sort=stars&order=desc"
        );
    }

    #[test]
    fn parse_response_returns_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("codeberg.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("codeberg.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be openable and labelled"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://codeberg.org/") && !r.url.contains("/api/v1/")),
            "results must link to the human-facing page, never the API endpoint"
        );
    }

    // Pinned against the response recorded on 2026-09-08 for
    // `q=http client`, so an upstream field rename fails loudly here instead
    // of quietly returning zero results to users.
    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("codeberg.json")).unwrap();
        assert_eq!(results[0].url, "https://codeberg.org/httpxyz/httpxyz");
        assert_eq!(results[0].title, "httpxyz/httpxyz");
        assert!(
            results[0]
                .description
                .starts_with("The friendly fork of a next generation HTTP client for Python. · ★ "),
            "unexpected description: {}",
            results[0].description
        );
        assert!(
            results[0].description.contains("· Python ·"),
            "the language should be part of the blurb: {}",
            results[0].description
        );
    }

    /// Paging past the last hit yields a well-formed body with an empty
    /// `data` array; the cache layer reads that as exhaustion, so it must not
    /// become an error.
    #[test]
    fn parse_response_treats_an_empty_data_array_as_exhaustion_not_an_error() {
        assert!(
            parse_response(r#"{"ok":true,"data":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_response_reports_a_failed_search_with_the_api_message() {
        let err = parse_response(r#"{"ok":false,"error":"keyword too short"}"#).unwrap_err();
        match err {
            EngineError::ParseError(detail) => assert!(
                detail.contains("keyword too short"),
                "the API's own message should survive: {detail}"
            ),
            other => panic!("expected a ParseError, got {other:?}"),
        }
    }

    /// Codeberg's 4xx bodies are `{"message":…,"url":…}` with no `ok` field,
    /// and a 422 is not classified as a block upstream — so it has to land
    /// here as a parse error, not an empty page.
    #[test]
    fn parse_response_reports_an_error_shaped_body_as_a_parse_error() {
        assert!(matches!(
            parse_response(
                r#"{"message":"Invalid sort mode","url":"https://codeberg.org/api/swagger"}"#
            ),
            Err(EngineError::ParseError(_))
        ));
    }

    // Hand-written rather than taken from the fixture: a `q=http client` page
    // happens to have descriptions throughout, but most of Codeberg does not
    // (49 of 50 repos were blank in one sampled page), and those rows still
    // have to be non-empty.
    #[test]
    fn describe_synthesises_a_blurb_when_the_repo_has_no_description() {
        let json = r#"{"ok":true,"data":[{
            "html_url": "https://codeberg.org/m13253/_cargo-index",
            "full_name": "m13253/_cargo-index",
            "description": "",
            "language": "",
            "stars_count": 0,
            "forks_count": 0,
            "topics": [],
            "updated_at": "2025-12-01T03:51:51+01:00"
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].description,
            "m13253/_cargo-index · ★ 0 · updated 2025-12-01"
        );
    }

    #[test]
    fn describe_caps_the_topic_list_and_the_overall_length() {
        let json = r#"{"ok":true,"data":[{
            "html_url": "https://codeberg.org/someone/loud",
            "full_name": "someone/loud",
            "description": "one   two\n\nthree",
            "language": "Rust",
            "stars_count": 7,
            "forks_count": 1234,
            "topics": ["a","b","c","d","e","f","g"],
            "updated_at": "2026-09-07T12:37:52+02:00"
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].description,
            "one two three · ★ 7 · ⑂ 1,234 · Rust · a, b, c, d, e"
        );
        assert!(results[0].description.chars().count() <= 300);
    }

    #[test]
    fn parse_response_drops_an_entry_with_no_openable_url() {
        let json = r#"{"ok":true,"data":[
            {"html_url":"","full_name":"someone/nowhere","description":"x",
             "language":"","stars_count":0,"forks_count":0,"topics":[],"updated_at":null},
            {"html_url":"https://codeberg.org/someone/real","full_name":"someone/real",
             "description":"x","language":"","stars_count":0,"forks_count":0,
             "topics":[],"updated_at":null}
        ]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://codeberg.org/someone/real");
    }

    /// Forgejo currently sends empty strings, but the fields are modelled as
    /// optional so a release that starts sending null can't blank a page.
    #[test]
    fn parse_response_tolerates_null_metadata_fields() {
        let json = r#"{"ok":true,"data":[{
            "html_url":"https://codeberg.org/someone/sparse",
            "full_name":"someone/sparse",
            "description":null,"language":null,"topics":null,"updated_at":null
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results[0].description, "someone/sparse · ★ 0");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(11814), "11,814");
    }

    #[ignore]
    #[tokio::test]
    async fn codeberg_search_live() {
        let results = Codeberg.search_results("http client", 0, 20).await.unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty())
        );
    }

    /// Multi-word queries only work because of `includeDesc=true`; without it
    /// this exact search returns zero hits upstream.
    #[ignore]
    #[tokio::test]
    async fn codeberg_multi_word_search_finds_something_live() {
        let results = Codeberg
            .search_results("rust http client", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn codeberg_paging_past_the_end_is_exhaustion_live() {
        let results = Codeberg
            .search_results("http client", PER_PAGE * 199, PER_PAGE)
            .await
            .unwrap();
        assert!(results.is_empty());
    }
}
