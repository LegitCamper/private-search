//! MDN Web Docs search, via developer.mozilla.org's public site-search API.
//!
//! Answers "what does MDN say about X" — for anything web-platform-shaped
//! (`fetch`, `grid-template-areas`, `IntersectionObserver`) MDN is the
//! canonical reference, and a general web engine buries it under tutorials
//! and StackOverflow reposts.
//!
//! Quirks worth knowing before changing anything here:
//!
//! * **`mdn_url` is site-relative** (`/en-US/docs/Web/API/Fetch_API`). It has
//!   to be prefixed with the origin or the result isn't openable.
//! * **The page size is fixed at 10 and paging is hard-capped at page 10.**
//!   Passing `size=20` is silently ignored (`metadata.size` still comes back
//!   `10`, verified live), and `page=11` is an HTTP **400** carrying a
//!   validation error, not an empty page. 400 isn't a status
//!   `classify_status` treats as a block, so it would surface as a confusing
//!   parse error — we stop short of the cap instead ([`MAX_RESULTS`]).
//!   `page=0` is likewise a 400, so the page number must stay 1-based.
//! * Paging *past the hit count* (but within the cap) is a well-formed 200
//!   with `"documents": []`, so exhaustion needs no `metadata.total`
//!   bookkeeping — an empty document list already means "no more".
//! * `summary` is documented as optional and a handful of stub/redirect pages
//!   ship without one, so we synthesise a blurb from the doc's slug, which
//!   encodes the area (`Web/API/fetch`).
//! * `summary` carries **literal** text — a `<feDropShadow>` arrives with real
//!   angle brackets, verified against live responses. The sibling `highlight`
//!   block is the HTML-escaped, `<mark>`-wrapped one; that's why we read
//!   `summary` and ignore `highlight`.
//!
//! No auth, no API key, and no rate limiting observed (15 requests back to
//! back all returned 200). A `User-Agent` isn't required either, though
//! [`crate::browser_client`] sends one anyway.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Mdn;

impl EngineInfo for Mdn {
    fn name(&self) -> &'static str {
        "MDN"
    }
}

const ORIGIN: &str = "https://developer.mozilla.org";

/// Fixed by the API: `metadata.size` is always 10 and the `size` parameter is
/// ignored, so this is not a knob we can turn.
const PER_PAGE: usize = 10;

/// The API refuses to serve past page 10 (HTTP 400). Anything at or beyond
/// this offset is exhaustion, not an error.
const MAX_RESULTS: usize = PER_PAGE * 10;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "{ORIGIN}/api/v1/search?q={}&locale=en-US&page={page}",
        encode(query)
    )
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    documents: Vec<Document>,
}

#[derive(Debug, Deserialize)]
struct Document {
    mdn_url: String,
    title: String,
    summary: Option<String>,
    /// Lowercased path without the locale prefix (`web/api/fetch_api`).
    /// Present on every live hit, but defaulted so a missing one degrades to
    /// "no synthesised blurb" rather than dropping the whole page.
    #[serde(default)]
    slug: String,
}

/// `/en-US/docs/Web/API/Fetch_API` -> `https://developer.mozilla.org/...`.
///
/// Returns `None` for anything that isn't an origin-relative doc path, so a
/// malformed entry is dropped instead of shipping a link that 404s.
fn absolute_url(mdn_url: &str) -> Option<String> {
    let path = mdn_url.trim();
    if path.starts_with(ORIGIN) {
        return Some(path.to_string());
    }
    // A protocol-relative or off-site value would silently become
    // `https://developer.mozilla.org//evil.example` under naive concatenation.
    if !path.starts_with('/') || path.starts_with("//") {
        return None;
    }
    Some(format!("{ORIGIN}{path}"))
}

/// Builds the result blurb.
///
/// MDN's own `summary` is the first sentence of the page and is exactly what
/// you want. When it's absent, the slug is the only signal left — and it's a
/// genuinely useful one, since it names the area a page belongs to
/// (`web/api/fetch_api` -> "Web › API › Fetch API").
fn describe(doc: &Document) -> String {
    let summary = doc.summary.as_deref().map(tidy).filter(|s| !s.is_empty());

    let text = match summary {
        Some(summary) => summary,
        None => match breadcrumb(&doc.slug) {
            Some(trail) => format!("MDN Web Docs reference — {trail}"),
            None => format!("MDN Web Docs reference — {}", tidy(&doc.title)),
        },
    };

    truncate(&text, 300)
}

/// `web/api/fetch_api` -> `Web › API › Fetch API`.
///
/// The slug is lowercased and underscore-joined, so it needs unflattening to
/// read as prose. Words in [`ACRONYMS`] are restored to their conventional
/// all-caps spelling; everything else is title-cased.
fn breadcrumb(slug: &str) -> Option<String> {
    let trail: Vec<String> = slug
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            segment
                .split('_')
                .filter(|word| !word.is_empty())
                .map(humanise_word)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|segment| !segment.is_empty())
        .collect();

    if trail.is_empty() {
        return None;
    }
    Some(trail.join(" › "))
}

/// Slug words that are acronyms upstream. A length heuristic doesn't work
/// here — `web` and `api` are both three letters but only one is an acronym.
const ACRONYMS: &[&str] = &[
    "api", "aria", "cors", "css", "dom", "html", "http", "https", "json", "jsx", "mathml", "rss",
    "svg", "uri", "url", "wasm", "webgl", "webrtc", "xhr", "xml", "xpath", "xslt",
];

fn humanise_word(word: &str) -> String {
    if ACRONYMS.contains(&word) {
        return word.to_ascii_uppercase();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "MDN returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(response
        .documents
        .iter()
        // A hit with no title has nothing to render as a link label, and one
        // with an unusable path isn't openable; either would violate the
        // non-empty contract, so drop it rather than ship a broken row.
        .filter_map(|doc| {
            let url = absolute_url(&doc.mdn_url)?;
            let title = tidy(&doc.title);
            if title.is_empty() {
                return None;
            }
            Some(RawResult {
                url,
                title,
                description: describe(doc),
            })
        })
        .collect())
}

#[async_trait]
impl SearchEngine for Mdn {
    /// `count` is ignored: the page size is fixed upstream at [`PER_PAGE`], so
    /// a given `start` always lands on the same page boundary — which is what
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

        // Not `super::get_json`: deserialising in the helper would either
        // duplicate the response shape or force a re-serialise round trip.
        // Going through `body_or_block` directly keeps the 403/429/5xx-to-
        // `Blocked` classification while leaving `parse_response` the one and
        // only place that knows the JSON shape — so the unit tests exercise
        // exactly the code the live path runs.
        let resp = crate::browser_client()
            .get(build_search_url(query, start))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        let body = crate::body_or_block(resp, "MDN").await?;
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
            build_search_url("fetch api", 0),
            "https://developer.mozilla.org/api/v1/search\
             ?q=fetch%20api&locale=en-US&page=1"
        );
        assert_eq!(
            build_search_url("fetch api", PER_PAGE - 1),
            "https://developer.mozilla.org/api/v1/search\
             ?q=fetch%20api&locale=en-US&page=1"
        );
    }

    #[test]
    fn build_search_url_advances_to_the_page_holding_start() {
        assert_eq!(
            build_search_url("fetch api", PER_PAGE),
            "https://developer.mozilla.org/api/v1/search\
             ?q=fetch%20api&locale=en-US&page=2"
        );
        assert_eq!(
            build_search_url("fetch api", PER_PAGE * 4 + 7),
            "https://developer.mozilla.org/api/v1/search\
             ?q=fetch%20api&locale=en-US&page=5"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii() {
        assert_eq!(
            build_search_url("café 日本語", 0),
            "https://developer.mozilla.org/api/v1/search\
             ?q=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E&locale=en-US&page=1"
        );
    }

    #[test]
    fn parse_response_turns_a_relative_mdn_url_into_an_absolute_one() {
        let json = r#"{"documents":[{
            "mdn_url": "/en-US/docs/Web/API/fetch",
            "title": "Window: fetch() method",
            "slug": "web/api/fetch",
            "summary": "The fetch() method starts the process of fetching a resource."
        }]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].url,
            "https://developer.mozilla.org/en-US/docs/Web/API/fetch"
        );
    }

    /// A protocol-relative or absolute off-site value would become
    /// `https://developer.mozilla.org//evil.example` under naive
    /// concatenation, i.e. an open redirect in a result row.
    #[test]
    fn parse_response_drops_an_entry_whose_path_is_not_origin_relative() {
        let json = r#"{"documents":[
            {"mdn_url":"//evil.example/x","title":"Bad","slug":"x","summary":"s"},
            {"mdn_url":"https://evil.example/x","title":"Bad","slug":"x","summary":"s"},
            {"mdn_url":"/en-US/docs/Web/API/fetch","title":"Good","slug":"web/api/fetch","summary":"s"}
        ]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good");
    }

    #[test]
    fn parse_response_returns_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("mdn.json")).unwrap();
        assert_eq!(results.len(), PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("mdn.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result must be openable and labelled"
        );
        assert!(
            results.iter().all(|r| r
                .url
                .starts_with("https://developer.mozilla.org/en-US/docs/")),
            "results must link to the human-facing doc, never /api/v1/"
        );
    }

    // Pinned against the response recorded on 2026-09-08 for `q=fetch api`,
    // so an upstream field rename fails loudly here instead of quietly
    // returning zero results to users.
    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("mdn.json")).unwrap();
        assert_eq!(
            results[0].url,
            "https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API"
        );
        assert_eq!(results[0].title, "Fetch API");
        assert!(
            results[0]
                .description
                .starts_with("The Fetch API provides an interface for fetching resources"),
            "unexpected description: {}",
            results[0].description
        );
    }

    /// Paging past the hit count yields a well-formed body with an empty
    /// `documents` array; the cache layer reads that as exhaustion, so it must
    /// not become an error.
    #[test]
    fn parse_response_treats_an_empty_document_list_as_exhaustion_not_an_error() {
        let json = r#"{"documents":[],"metadata":{"took_ms":4,"size":10,"page":2,
            "total":{"value":0,"relation":"eq"}},"suggestions":[]}"#;
        assert!(parse_response(json).unwrap().is_empty());
    }

    #[test]
    fn parse_response_reports_a_changed_api_shape_as_a_parse_error() {
        let json = r#"{"errors":{"page":[{"code":"invalid",
            "message":"Ensure this value is less than or equal to 10."}]}}"#;
        assert!(matches!(
            parse_response(json),
            Err(EngineError::ParseError(_))
        ));
    }

    // Hand-written rather than taken from the fixture: every hit on a popular
    // query has a summary, but stub and redirect pages ship without one and
    // that row still has to be non-empty.
    #[test]
    fn describe_synthesises_a_blurb_from_the_slug_when_the_summary_is_missing() {
        let json = r#"{"documents":[
            {"mdn_url":"/en-US/docs/Web/API/Fetch_API","title":"Fetch API",
             "slug":"web/api/fetch_api","summary":null},
            {"mdn_url":"/en-US/docs/Web/CSS/grid_template_areas",
             "title":"grid-template-areas","slug":"web/css/grid_template_areas","summary":"   "}
        ]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].description,
            "MDN Web Docs reference — Web › API › Fetch API"
        );
        // A whitespace-only summary is as good as absent.
        assert_eq!(
            results[1].description,
            "MDN Web Docs reference — Web › CSS › Grid Template Areas"
        );
    }

    #[test]
    fn describe_falls_back_to_the_title_when_there_is_no_slug_either() {
        let json = r#"{"documents":[
            {"mdn_url":"/en-US/docs/Web/API/fetch","title":"Window: fetch() method"}
        ]}"#;

        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].description,
            "MDN Web Docs reference — Window: fetch() method"
        );
    }

    #[test]
    fn describe_collapses_whitespace_and_caps_the_length() {
        let long = "word ".repeat(200);
        let json = format!(
            r#"{{"documents":[{{"mdn_url":"/en-US/docs/Web/API/x","title":"x",
                "slug":"web/api/x","summary":"one   two\n\nthree {long}"}}]}}"#
        );

        let description = &parse_response(&json).unwrap()[0].description;
        assert!(description.starts_with("one two three word word"));
        assert!(
            description.chars().count() <= 301,
            "300 chars plus ellipsis"
        );
        assert!(description.ends_with('…'));
    }

    #[test]
    fn breadcrumb_upper_cases_platform_acronyms_but_title_cases_words() {
        assert_eq!(
            breadcrumb("web/http/guides/fetch_metadata").unwrap(),
            "Web › HTTP › Guides › Fetch Metadata"
        );
        assert!(breadcrumb("").is_none());
    }

    #[tokio::test]
    async fn search_results_stops_at_the_page_cap_without_a_request() {
        // Page 11 is a 400 upstream, so this must short-circuit rather than
        // hit the network — no `#[ignore]`, precisely because it makes no
        // request.
        let results = Mdn
            .search_results("fetch", MAX_RESULTS, PER_PAGE)
            .await
            .unwrap();
        assert!(results.is_empty());

        let past_cap = Mdn
            .search_results("fetch", MAX_RESULTS * 3, PER_PAGE)
            .await
            .unwrap();
        assert!(past_cap.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn mdn_search_live() {
        let results = Mdn.search_results("fetch api", 0, 10).await.unwrap();
        assert_eq!(results.len(), PER_PAGE);
        assert!(results.iter().all(|r| {
            r.url
                .starts_with("https://developer.mozilla.org/en-US/docs/")
                && !r.title.is_empty()
                && !r.description.is_empty()
        }));
    }

    #[ignore]
    #[tokio::test]
    async fn mdn_search_pagination_live() {
        let page1 = Mdn.search_results("fetch api", 0, 10).await.unwrap();
        let page2 = Mdn.search_results("fetch api", PER_PAGE, 10).await.unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "page 2 should advance, not repeat page 1"
        );
    }

    /// The last servable page is 10; page 11 is a 400 we must never send.
    #[ignore]
    #[tokio::test]
    async fn mdn_search_last_page_within_the_cap_still_returns_results_live() {
        let last = Mdn
            .search_results("fetch api", MAX_RESULTS - PER_PAGE, 10)
            .await
            .unwrap();
        assert!(!last.is_empty());
    }
}
