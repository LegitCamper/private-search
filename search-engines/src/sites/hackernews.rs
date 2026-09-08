//! Hacker News, via the public Algolia search index at `hn.algolia.com`.
//!
//! This is the engine for "what did HN say about X" — the discussion is
//! usually the thing the user is after, so every result's description
//! carries the score, the comment count and a direct link to the thread.
//!
//! Two quirks of the Algolia API are worth knowing before touching this:
//!
//! * `page` is **0-based**, unlike nearly every other API in this module and
//!   unlike [`super::page_number`]. Off-by-one here doesn't error — it
//!   silently re-serves page 1 forever.
//! * `hits[].url` is `null` for text posts (Ask HN, Show HN without a link,
//!   Tell HN). Those stories live only on HN itself, so we fall back to the
//!   `news.ycombinator.com/item?id=…` permalink rather than emitting an
//!   empty URL.
//!
//! Algolia also caps the index at 1000 hits per query (50 pages of 20), and
//! reports that ceiling as `nbPages`; asking for a page past it returns an
//! HTTP 200 with an explanatory `message` and zero hits.
//!
//! One deliberate deviation from the usual all-`https` result contract: HN
//! is old enough that a minority of indexed stories still link out over
//! plain `http`. Those are passed through verbatim rather than rewritten to
//! `https`, because an upgrade is a guess — an http-only host answers the
//! rewritten URL with a connection failure instead of the page. Everything
//! we synthesise ourselves (the item permalinks) is `https`.

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_html, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct HackerNews;

impl EngineInfo for HackerNews {
    fn name(&self) -> &'static str {
        "Hacker News"
    }
}

/// Algolia's default and our explicit request size. The API honours
/// `hitsPerPage`, but 20 keeps us inside the 1000-hit ceiling at a round 50
/// pages and matches the other engines' page size.
const HN_HITS_PER_PAGE: usize = 20;

fn build_search_url(query: &str, start: usize) -> String {
    // `page_number` is 1-based for the majority of APIs; Algolia is 0-based.
    let page = page_number(start, HN_HITS_PER_PAGE) - 1;
    format!(
        "https://hn.algolia.com/api/v1/search?query={}&tags=story&page={page}&hitsPerPage={HN_HITS_PER_PAGE}",
        encode(query)
    )
}

/// The HN web page for a story, which is also the comment thread.
fn item_permalink(object_id: &str) -> String {
    format!("https://news.ycombinator.com/item?id={object_id}")
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<Hit>,
    /// Echo of the requested page, 0-based.
    #[serde(default)]
    page: usize,
    /// Total pages available for this query, bounded by Algolia's 1000-hit
    /// pagination limit.
    #[serde(default, rename = "nbPages")]
    nb_pages: usize,
}

#[derive(Deserialize)]
struct Hit {
    /// The HN item id, as a string. Present on every hit.
    #[serde(rename = "objectID")]
    object_id: String,
    #[serde(default)]
    title: Option<String>,
    /// `null` for text-only posts — see the module docs.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    num_comments: Option<i64>,
    /// RFC 3339, e.g. `2018-06-11T16:27:54Z`.
    #[serde(default)]
    created_at: Option<String>,
    /// The self-post body, as HTML.
    #[serde(default)]
    story_text: Option<String>,
}

/// Renders `html` down to plain text: drops tags and decodes entities.
///
/// HN self-post bodies are HTML (`<p>` paragraph breaks, `&#x27;` for an
/// apostrophe), which would otherwise land verbatim in a result snippet.
/// Reuses `scraper` rather than hand-rolling an entity table.
fn strip_html(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    // Join with spaces: `<p>` carries the only word boundary between
    // paragraphs, and concatenating text nodes directly would fuse the last
    // word of one onto the first of the next.
    tidy(&fragment.root_element().text().collect::<Vec<_>>().join(" "))
}

/// Builds the one-line snippet: HN's own metadata is the useful part here,
/// since the linked page's own description isn't in the index.
fn describe(hit: &Hit, has_own_url: bool) -> String {
    let mut parts = Vec::new();

    parts.push(format!("{} points", hit.points.unwrap_or(0)));
    parts.push(format!("{} comments", hit.num_comments.unwrap_or(0)));

    if let Some(author) = hit.author.as_deref().filter(|a| !a.is_empty()) {
        parts.push(format!("by {author}"));
    }
    // `created_at` is a full timestamp; the date alone is what a reader
    // scanning results actually uses.
    if let Some(date) = hit
        .created_at
        .as_deref()
        .and_then(|t| t.split('T').next())
        .filter(|d| !d.is_empty())
    {
        parts.push(date.to_string());
    }
    // Only worth spending snippet space on when the result URL points at the
    // linked article; for a text post the result URL *is* the thread.
    if has_own_url {
        parts.push(format!("discussion: {}", item_permalink(&hit.object_id)));
    }

    let mut description = parts.join(" · ");
    if let Some(text) = hit.story_text.as_deref().filter(|t| !t.trim().is_empty()) {
        let body = strip_html(text);
        if !body.is_empty() {
            description.push_str(" — ");
            description.push_str(&body);
        }
    }

    description
}

pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "Hacker News returned a body that isn't the JSON we expect ({e}); \
             the Algolia API shape may have changed"
        ))
    })?;

    // Past the end of the index: treat as exhaustion rather than paging
    // forever. Guarded on `nb_pages > 0` so a response that omits the field
    // doesn't make us discard hits we were actually served.
    if response.nb_pages > 0 && response.page >= response.nb_pages {
        return Ok(Vec::new());
    }

    let results = response
        .hits
        .iter()
        .filter_map(|hit| {
            // A story with no title can't produce a non-empty result row;
            // `tags=story` should exclude these, but comments carry
            // `comment_text` instead of `title` if the filter ever slips.
            let title = tidy(hit.title.as_deref().unwrap_or_default());
            if title.is_empty() {
                return None;
            }

            let own_url = hit
                .url
                .as_deref()
                .map(str::trim)
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
            let url = own_url
                .map(str::to_string)
                .unwrap_or_else(|| item_permalink(&hit.object_id));

            let description = describe(hit, own_url.is_some());

            Some(RawResult {
                url,
                title,
                description: truncate(&tidy(&description), 300),
            })
        })
        .collect();

    Ok(results)
}

#[async_trait]
impl SearchEngine for HackerNews {
    /// `count` is ignored: the page size is pinned to [`HN_HITS_PER_PAGE`] so
    /// that `start` maps cleanly onto Algolia's fixed-size pages.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // `get_html` is just "GET and hand back the body text"; we want the
        // raw string so `parse_response` stays independently testable.
        let body = get_html(&build_search_url(query, start), "Hacker News").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_algolias_zero_based_first_page() {
        assert_eq!(
            build_search_url("rust programming", 0),
            "https://hn.algolia.com/api/v1/search?query=rust%20programming\
             &tags=story&page=0&hitsPerPage=20"
        );
    }

    /// The whole point of this test: `page_number` is 1-based, Algolia is
    /// 0-based. Forgetting the `- 1` re-serves page 1 for every page, which
    /// looks like working pagination until you notice the duplicates.
    #[test]
    fn build_search_url_converts_the_one_based_page_number_to_a_zero_based_page() {
        assert_eq!(
            build_search_url("rust", HN_HITS_PER_PAGE),
            "https://hn.algolia.com/api/v1/search?query=rust&tags=story&page=1&hitsPerPage=20"
        );
        assert_eq!(
            build_search_url("rust", HN_HITS_PER_PAGE * 2 + 7),
            "https://hn.algolia.com/api/v1/search?query=rust&tags=story&page=2&hitsPerPage=20"
        );
        // Still page 0 anywhere inside the first page's worth of results.
        assert_eq!(
            build_search_url("rust", HN_HITS_PER_PAGE - 1),
            "https://hn.algolia.com/api/v1/search?query=rust&tags=story&page=0&hitsPerPage=20"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_queries() {
        assert_eq!(
            build_search_url("café au lait", 0),
            "https://hn.algolia.com/api/v1/search?query=caf%C3%A9%20au%20lait\
             &tags=story&page=0&hitsPerPage=20"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        assert_eq!(results.len(), HN_HITS_PER_PAGE);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every result row must be renderable"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://") || r.url.starts_with("http://")),
            "result URLs must be absolute, never relative or an API endpoint"
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_recorded_response() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        assert_eq!(
            results[0].url,
            "https://simplabs.com/blog/2018/06/11/actix.html"
        );
        assert_eq!(
            results[0].title,
            "Actix – an actor framework for the Rust programming language"
        );
        assert_eq!(
            results[0].description,
            "13 points · 1 comments · by donmcc · 2018-06-11 · \
             discussion: https://news.ycombinator.com/item?id=17285692"
        );
    }

    /// The fixture's Ask HN post has `"url": null` — the case that would
    /// otherwise emit a result nobody can click.
    #[test]
    fn parse_response_falls_back_to_the_hn_permalink_when_a_story_has_no_url() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        let ask_hn = results
            .iter()
            .find(|r| r.title.starts_with("Ask HN: Why was the"))
            .expect("fixture should contain the null-url Ask HN story");

        assert_eq!(ask_hn.url, "https://news.ycombinator.com/item?id=32585149");
        // The result URL is already the thread, so repeating it would waste
        // snippet space.
        assert!(!ask_hn.description.contains("discussion:"));
    }

    #[test]
    fn parse_response_renders_story_text_as_plain_text() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        let ask_hn = results
            .iter()
            .find(|r| r.title.starts_with("Ask HN: Why was the"))
            .unwrap();

        assert!(ask_hn.description.contains("I indirectly started asking"));
        assert!(
            !ask_hn.description.contains('<') && !ask_hn.description.contains("&#x"),
            "HN self-post bodies are HTML and must be rendered down to text: {}",
            ask_hn.description
        );
    }

    /// Old stories legitimately carry `http://` outbound links. Pinning this
    /// keeps a future "just force https" tidy-up from silently breaking the
    /// http-only hosts it would be applied to.
    #[test]
    fn parse_response_passes_a_plain_http_story_link_through_unchanged() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        let nalgebra = results
            .iter()
            .find(|r| r.title.starts_with("Nalgebra"))
            .expect("fixture should contain the http-only Nalgebra story");
        assert_eq!(nalgebra.url, "http://nalgebra.org/");
    }

    #[test]
    fn parse_response_truncates_long_descriptions() {
        let results = parse_response(&fixture("hackernews.json")).unwrap();
        assert!(results.iter().all(|r| r.description.chars().count() <= 301));
    }

    #[test]
    fn parse_response_treats_a_well_formed_empty_result_set_as_no_results() {
        let empty = r#"{"hits":[],"nbHits":0,"page":0,"nbPages":0,"hitsPerPage":20,"query":"asdkjfhaslkdjfh"}"#;
        assert!(parse_response(empty).unwrap().is_empty());
    }

    /// Algolia answers a request past its 1000-hit ceiling with a 200 and an
    /// explanatory `message`, not an error — exhaustion, not a failure.
    #[test]
    fn parse_response_returns_no_results_for_a_page_past_the_pagination_limit() {
        let past_end = r#"{"hits":[],"nbHits":0,"page":60,"nbPages":0,"hitsPerPage":20,
            "message":"you can only fetch the 1000 hits for this query","query":"rust"}"#;
        assert!(parse_response(past_end).unwrap().is_empty());
    }

    /// Defensive: if Algolia ever serves hits on a page it also reports as
    /// out of range, prefer the bound so the caller stops paging.
    #[test]
    fn parse_response_stops_once_the_requested_page_exceeds_nb_pages() {
        let over = r#"{"hits":[{"objectID":"1","title":"t","url":"https://e.com"}],
            "page":50,"nbPages":50,"hitsPerPage":20}"#;
        assert!(parse_response(over).unwrap().is_empty());
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_the_expected_json() {
        assert!(parse_response("<html>rate limited</html>").is_err());
    }

    #[test]
    fn strip_html_drops_tags_and_decodes_entities() {
        assert_eq!(
            strip_html("I don&#x27;t like C.<p>Rust &amp; Go are fine."),
            "I don't like C. Rust & Go are fine."
        );
    }

    #[ignore]
    #[tokio::test]
    async fn hackernews_live_search_returns_results() {
        let results = HackerNews
            .search_results("rust programming", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty())
        );
    }

    #[ignore]
    #[tokio::test]
    async fn hackernews_live_second_page_does_not_repeat_the_first() {
        let page1 = HackerNews.search_results("rust", 0, 20).await.unwrap();
        let page2 = HackerNews
            .search_results("rust", HN_HITS_PER_PAGE, 20)
            .await
            .unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2
                .iter()
                .all(|r2| !page1.iter().any(|r1| r1.url == r2.url)),
            "0-based page conversion is wrong if page 2 repeats page 1"
        );
    }
}
