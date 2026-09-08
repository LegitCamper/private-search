//! RubyGems — the canonical registry for Ruby packages (`gem install`).
//!
//! Answers "which gem is called X", so a query for `sidekiq` surfaces the
//! gem page itself rather than blog posts about it.
//!
//! Two quirks worth knowing:
//!
//! * The search endpoint returns a **bare JSON array**, not the usual
//!   `{"results": [...], "total": N}` wrapper. There is no total count and no
//!   "has more pages" flag anywhere in the response.
//! * Page size is fixed at 30 (`count` is ignored — the API has no per-page
//!   parameter), and paging past the last hit returns `[]` rather than a 404.
//!   That empty array is exactly our exhaustion signal, so the missing total
//!   count costs us nothing.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_html, page_number, tidy};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct RubyGems;

impl EngineInfo for RubyGems {
    fn name(&self) -> &'static str {
        "RubyGems"
    }
}

/// Fixed by the API — there is no per-page parameter to raise it.
const PER_PAGE: usize = 30;

/// Longest description we emit, matching the other site adapters.
const MAX_DESCRIPTION: usize = 300;

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://rubygems.org/api/v1/search.json?query={}&page={}",
        encode(query),
        page_number(start, PER_PAGE)
    )
}

/// One element of the search array. Everything but `name` is optional:
/// `project_uri`, `info` and `authors` are all nullable in the schema, and
/// treating them as required would turn one odd gem into a whole-page parse
/// failure.
#[derive(Deserialize)]
struct Gem {
    #[serde(default)]
    name: String,
    /// The gem's summary line. Named `info` for historical reasons.
    #[serde(default)]
    info: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    downloads: Option<u64>,
    /// Human-facing gem page, e.g. `https://rubygems.org/gems/rails`.
    #[serde(default)]
    project_uri: Option<String>,
    #[serde(default)]
    authors: Option<String>,
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// `project_uri` is the gem's page on rubygems.org, but it is nullable and we
/// must never hand back an API URL — fall back to the page it would have
/// pointed at.
fn gem_url(gem: &Gem) -> String {
    match non_empty(&gem.project_uri) {
        Some(uri) if uri.starts_with("https://") => uri.to_string(),
        _ => format!("https://rubygems.org/gems/{}", encode(&gem.name)),
    }
}

/// Groups a download count with commas — an ungrouped nine-digit number is
/// unreadable in a one-line result row, and download count is the main
/// signal users skim these results for.
fn group_digits(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Truncates to an exact character ceiling, counting the ellipsis as part of
/// that ceiling. The shared helper intentionally appends its ellipsis after
/// taking `max` characters, which is useful for snippets but not for this
/// adapter's strict `MAX_DESCRIPTION` contract.
fn truncate_exact(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }

    let mut output: String = text.chars().take(max - 1).collect();
    output.push('…');
    output
}

/// Summary plus version and downloads. The description is required to be
/// non-empty, so a gem with no summary gets one synthesized from whatever
/// metadata it does have.
fn describe(gem: &Gem) -> String {
    let summary = match non_empty(&gem.info) {
        Some(info) => tidy(info),
        None => match non_empty(&gem.authors) {
            Some(authors) => format!("Ruby gem {} by {}.", gem.name, tidy(authors)),
            None => format!("Ruby gem {}.", gem.name),
        },
    };

    let mut facts = Vec::new();
    if let Some(version) = non_empty(&gem.version) {
        facts.push(format!("v{version}"));
    }
    if let Some(downloads) = gem.downloads {
        facts.push(format!("{} downloads", group_digits(downloads)));
    }

    if facts.is_empty() {
        return truncate_exact(&summary, MAX_DESCRIPTION);
    }

    // Trim the summary rather than the metadata: a long summary would
    // otherwise push the version and download count past the cut, which is
    // the part of the line that isn't recoverable from the title.
    let facts = facts.join(", ");
    let budget = MAX_DESCRIPTION.saturating_sub(facts.chars().count() + 3);
    let combined = format!("{} — {facts}", truncate_exact(&summary, budget));
    truncate_exact(&combined, MAX_DESCRIPTION)
}

pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let gems: Vec<Gem> = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "RubyGems returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(gems
        .into_iter()
        // A nameless gem can't produce a title or a usable URL; the registry
        // has never served one, but emitting a blank row would be worse.
        .filter(|gem| !gem.name.trim().is_empty())
        .map(|gem| RawResult {
            url: gem_url(&gem),
            title: gem.name.trim().to_string(),
            description: describe(&gem),
        })
        .collect())
}

#[async_trait]
impl SearchEngine for RubyGems {
    /// `count` is ignored: the API always returns 30 per page and offers no
    /// knob to change it.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let body = get_html(&build_search_url(query, start), "RubyGems").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_the_first_page_for_a_zero_start() {
        assert_eq!(
            build_search_url("rails", 0),
            "https://rubygems.org/api/v1/search.json?query=rails&page=1"
        );
        assert_eq!(
            build_search_url("rails", PER_PAGE - 1),
            "https://rubygems.org/api/v1/search.json?query=rails&page=1"
        );
    }

    #[test]
    fn build_search_url_advances_the_page_once_start_passes_the_page_size() {
        assert_eq!(
            build_search_url("rails", PER_PAGE),
            "https://rubygems.org/api/v1/search.json?query=rails&page=2"
        );
        assert_eq!(
            build_search_url("rails", PER_PAGE * 4 + 7),
            "https://rubygems.org/api/v1/search.json?query=rails&page=5"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_queries() {
        assert_eq!(
            build_search_url("café rails", 0),
            "https://rubygems.org/api/v1/search.json?query=caf%C3%A9%20rails&page=1"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("rubygems.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("rubygems.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be renderable; a blank row is worse than a \
             missing one"
        );
        assert!(
            results.iter().all(|r| r.url.starts_with("https://")),
            "urls must be absolute pages a human can open"
        );
        assert!(
            results.iter().all(|r| !r.url.contains("/api/v1/")),
            "an API url would be useless to the person clicking the result"
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("rubygems.json")).unwrap();
        assert_eq!(results[0].url, "https://rubygems.org/gems/rails");
        assert_eq!(results[0].title, "rails");
        assert!(results[0].description.contains("full-stack web framework"));
        assert!(results[0].description.contains("downloads"));
    }

    #[test]
    fn parse_response_truncates_a_long_description_but_keeps_the_metadata() {
        let results = parse_response(&fixture("rubygems.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| r.description.chars().count() <= MAX_DESCRIPTION)
        );
        assert!(
            results.iter().all(|r| r.description.contains("downloads")),
            "version/download metadata must survive the length cut"
        );
    }

    // The live API has never been observed serving a gem without a summary,
    // so this shape is synthetic — but `info` is nullable in the schema and
    // an empty description would break the result contract.
    #[test]
    fn parse_response_synthesizes_a_description_for_a_gem_with_no_summary() {
        let json = r#"[
            {"name": "quiet", "info": null, "version": "0.1.0",
             "downloads": 12, "project_uri": null, "authors": "A. Coder"},
            {"name": "quieter", "info": "   ", "version": null,
             "downloads": null, "authors": null}
        ]"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 2);

        assert_eq!(results[0].url, "https://rubygems.org/gems/quiet");
        assert_eq!(
            results[0].description,
            "Ruby gem quiet by A. Coder. — v0.1.0, 12 downloads"
        );

        // Nothing but a name to work with, and still non-empty.
        assert_eq!(results[1].description, "Ruby gem quieter.");
    }

    /// Paging past the last hit returns `[]` with a 200, which the cache
    /// layer reads as exhaustion — it must not surface as a parse error.
    #[test]
    fn parse_response_treats_an_empty_array_as_exhaustion_not_an_error() {
        assert!(parse_response("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_the_expected_array() {
        assert!(parse_response("<html>rate limited</html>").is_err());
        assert!(parse_response(r#"{"results": []}"#).is_err());
    }

    #[test]
    fn group_digits_makes_large_download_counts_readable() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(784_840_795), "784,840,795");
    }

    #[ignore]
    #[tokio::test]
    async fn test_rubygems_search_live() {
        let results = RubyGems.search_results("rails", 0, 30).await.unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|r| !r.description.is_empty()));
    }

    #[ignore]
    #[tokio::test]
    async fn test_rubygems_pagination_live() {
        let page1 = RubyGems.search_results("rails", 0, 30).await.unwrap();
        let page2 = RubyGems
            .search_results("rails", PER_PAGE, 30)
            .await
            .unwrap();

        assert!(!page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "page 2 should advance, not repeat page 1"
        );
    }
}
