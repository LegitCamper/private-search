use crate::{
    EngineError, EngineInfo, ImageEngine, RawImage, RawResult, SearchEngine, new_rand_client,
    parse_images, parse_search,
};
use async_trait::async_trait;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

#[derive(Clone)]
pub struct Brave;

impl EngineInfo for Brave {
    fn name(&self) -> &'static str {
        "Brave"
    }
}

/// Results per `offset` increment on Brave's web results page — confirmed
/// empirically, not documented by Brave.
const BRAVE_RESULTS_PER_PAGE: usize = 20;

fn build_search_url(query: &str, start: usize) -> String {
    let page = start / BRAVE_RESULTS_PER_PAGE;
    let query = utf8_percent_encode(query, NON_ALPHANUMERIC);
    let mut url = format!("https://search.brave.com/search?q={query}");
    if page > 0 {
        url.push_str(&format!("&offset={page}"));
    }
    url
}

fn build_image_search_url(query: &str) -> String {
    let query = utf8_percent_encode(query, NON_ALPHANUMERIC);
    format!("https://search.brave.com/images?q={query}")
}

// A real Brave results page always has the SvelteKit app shell's
// `id="search-page"` container, even with 0 hits; only a block/captcha
// interstitial omits it. Without this check a block silently parses to an
// empty Vec, indistinguishable from genuine exhaustion. (Brave's pre-2026
// markup used `id="results"`; the SvelteKit redesign dropped it.)
fn looks_like_search_results(html: &str) -> bool {
    html.contains(r#"id="search-page""#)
}

fn looks_like_image_results(html: &str) -> bool {
    html.contains("image-result")
}

#[async_trait]
impl SearchEngine for Brave {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let resp = new_rand_client()
            .get(build_search_url(query, start))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        let html = resp.text().await.map_err(EngineError::ReqwestError)?;
        if !looks_like_search_results(&html) {
            return Err(EngineError::ParseError(
                "Brave response didn't look like real results (likely blocked)".into(),
            ));
        }

        parse_search_response(&html)
    }
}

pub fn parse_search_response(html: &str) -> Result<Vec<RawResult>, EngineError> {
    // Brave's SvelteKit page tags every snippet with a `data-type`; organic
    // web hits are `data-type="web"`, which also excludes `cluster`/news/
    // video rollup blocks (the old markup's `#results` wrapper and
    // `.standalone` class are gone).
    Ok(parse_search(
        html,
        r#".snippet[data-pos][data-type="web"]"#,
        ".title",
        "a",
        ".generic-snippet, .video-snippet > .snippet-description",
    ))
}

#[async_trait]
impl ImageEngine for Brave {
    /// `start`/`count` are unused: Brave's static image page always returns
    /// the same first batch, since deeper pages load via a signed,
    /// session-bound API this doesn't replicate.
    async fn search_images(
        &self,
        query: &str,
        _start: usize,
        _count: usize,
    ) -> Result<Vec<RawImage>, EngineError> {
        let resp = new_rand_client()
            .get(build_image_search_url(query))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        let html = resp.text().await.map_err(EngineError::ReqwestError)?;
        if !looks_like_image_results(&html) {
            return Err(EngineError::ParseError(
                "Brave image response didn't look like real results (likely blocked)".into(),
            ));
        }

        parse_image_response(&html)
    }
}

pub fn parse_image_response(html: &str) -> Result<Vec<RawImage>, EngineError> {
    // Brave mixes a non-image "Find elsewhere" navigation card into the same
    // `.image-result` class as real image tiles. That card has no `<img>`,
    // so including it produces a broken entry with an empty `url` and
    // `title` in every live Brave image search — exclude it by its second
    // class instead.
    Ok(parse_images(
        html,
        ".image-result:not(.image-result-search-elsewhere)",
        ".image-metadata-title",
        "img",
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn build_search_url_omits_offset_on_first_page() {
        assert_eq!(
            build_search_url("rust async", 0),
            "https://search.brave.com/search?q=rust%20async"
        );
        assert_eq!(
            build_search_url("rust async", BRAVE_RESULTS_PER_PAGE - 1),
            "https://search.brave.com/search?q=rust%20async"
        );
    }

    #[test]
    fn build_search_url_adds_offset_for_later_pages() {
        assert_eq!(
            build_search_url("rust async", BRAVE_RESULTS_PER_PAGE),
            "https://search.brave.com/search?q=rust%20async&offset=1"
        );
        assert_eq!(
            build_search_url("rust async", BRAVE_RESULTS_PER_PAGE * 2 + 5),
            "https://search.brave.com/search?q=rust%20async&offset=2"
        );
    }

    #[test]
    fn build_search_url_encodes_reserved_characters() {
        assert_eq!(
            build_search_url("AT&T c++ #tag", 0),
            "https://search.brave.com/search?q=AT%26T%20c%2B%2B%20%23tag"
        );
    }

    /// Non-ASCII queries must survive a full percent-encode round trip —
    /// regression guard for "encoding support" (unicode search terms are a
    /// normal, not edge-case, input for a search engine).
    #[test]
    fn build_search_url_encodes_non_ascii_query() {
        assert_eq!(
            build_search_url("café 日本語", 0),
            "https://search.brave.com/search?q=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E"
        );
    }

    #[test]
    fn looks_like_search_results_rejects_a_block_page() {
        assert!(looks_like_search_results(
            r#"<div id="search-page">...</div>"#
        ));
        assert!(!looks_like_search_results(
            "<html><body>please verify you're human</body></html>"
        ));
    }

    #[test]
    fn looks_like_image_results_rejects_a_block_page() {
        assert!(looks_like_image_results(
            r#"<div class="image-result">...</div>"#
        ));
        assert!(!looks_like_image_results(
            "<html><body>please verify you're human</body></html>"
        ));
    }

    // Mirrors Brave's SvelteKit markup (confirmed against a live fetch):
    // snippets are tagged with `data-type`, and only `data-type="web"` rows
    // are organic results — `cluster` (and news/video rollups) share the
    // same `.snippet` class but must be excluded.
    const SEARCH_FIXTURE: &str = r#"
        <div id="search-page">
            <div class="snippet svelte-x" data-pos="1" data-type="web">
                <a href="https://example.com/rust">
                    <div class="title search-snippet-title">Rust Programming Language</div>
                </a>
                <div class="generic-snippet">A systems language for reliable software.</div>
            </div>
            <div class="snippet svelte-x" data-pos="2" data-type="web">
                <a href="https://example.com/video">
                    <div class="title search-snippet-title">Rust in 100 seconds</div>
                </a>
                <div class="video-snippet">
                    <div class="snippet-description">A quick video overview.</div>
                </div>
            </div>
            <div class="snippet svelte-x" data-pos="3" data-type="cluster">
                <a href="https://example.com/ignored">
                    <div class="title">Should be excluded</div>
                </a>
                <div class="generic-snippet">Cluster rollups aren't organic results.</div>
            </div>
        </div>
    "#;

    #[test]
    fn parse_search_response_extracts_results_and_skips_non_web_snippets() {
        let results = parse_search_response(SEARCH_FIXTURE).unwrap();

        assert_eq!(results.len(), 2);

        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(
            results[0].description,
            "A systems language for reliable software."
        );

        assert_eq!(results[1].url, "https://example.com/video");
        assert_eq!(results[1].title, "Rust in 100 seconds");
        assert_eq!(results[1].description, "A quick video overview.");

        assert!(
            results
                .iter()
                .all(|r| r.url != "https://example.com/ignored")
        );
    }

    const IMAGE_FIXTURE: &str = r#"
        <div class="image-result">
            <img src="https://imgs.example.com/rust-logo.png">
            <div class="image-metadata-title">Rust Logo</div>
        </div>
        <div class="image-result">
            <img src="https://imgs.example.com/ferris.png">
            <div class="image-metadata-title">Ferris the Crab</div>
        </div>
    "#;

    #[test]
    fn parse_image_response_extracts_images() {
        let images = parse_image_response(IMAGE_FIXTURE).unwrap();

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].url, "https://imgs.example.com/rust-logo.png");
        assert_eq!(images[0].title, "Rust Logo");
        assert_eq!(images[1].url, "https://imgs.example.com/ferris.png");
        assert_eq!(images[1].title, "Ferris the Crab");
    }

    // These pin the parsers against real Brave markup captured on
    // 2026-08-19 (`tests/fixtures/brave/`), so the next upstream redesign
    // fails CI with a loud assertion instead of silently returning zero
    // results to users, the way the SvelteKit rewrite did. Fixtures are read
    // straight off disk (not through `fixtures::cached_html`), so a missing
    // file fails the test instead of quietly falling back to a live fetch.
    fn read_brave_fixture(relative: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing fixture {relative}: {e}"))
    }

    #[test]
    fn looks_like_search_results_accepts_the_real_brave_fixture() {
        let html = read_brave_fixture("brave/search_p0.html");
        assert!(looks_like_search_results(&html));
    }

    #[test]
    fn parse_search_response_returns_twenty_results_from_the_real_fixture() {
        let html = read_brave_fixture("brave/search_p0.html");
        let results = parse_search_response(&html).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn parse_search_response_extracts_non_empty_fields_from_the_real_fixture() {
        let html = read_brave_fixture("brave/search_p0.html");
        let results = parse_search_response(&html).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty()),
            "a selector that matches containers but extracts nothing would \
             leave url/title empty — the exact failure mode that made Brave \
             return empty strings"
        );
    }

    #[test]
    fn parse_search_response_first_result_matches_the_real_fixture() {
        let html = read_brave_fixture("brave/search_p0.html");
        let results = parse_search_response(&html).unwrap();
        assert_eq!(results[0].url, "https://rust-lang.github.io/async-book/");
        assert_eq!(
            results[0].title,
            "Introduction - Asynchronous Programming in Rust"
        );
    }

    #[test]
    fn parse_search_response_page_two_does_not_repeat_page_one_on_the_real_fixtures() {
        let page1 = parse_search_response(&read_brave_fixture("brave/search_p0.html")).unwrap();
        let page2 = parse_search_response(&read_brave_fixture("brave/search_p1.html")).unwrap();

        assert_eq!(page2.len(), 20);
        assert!(
            page2
                .iter()
                .all(|r2| !page1.iter().any(|r1| r1.url == r2.url)),
            "pagination should advance, not repeat page 1's results"
        );
    }

    #[test]
    fn parse_image_response_returns_fifty_images_from_the_real_fixture() {
        let html = read_brave_fixture("brave/images_p0.html");
        assert!(looks_like_image_results(&html));

        let images = parse_image_response(&html).unwrap();
        assert_eq!(images.len(), 50);
        assert!(
            images
                .iter()
                .all(|i| !i.url.is_empty() && !i.title.is_empty()),
            "a selector that matches Brave's non-image \"Find elsewhere\" \
             filler card would leave url/title empty on that entry"
        );
    }

    use crate::fixtures::cached_html;

    #[ignore]
    #[tokio::test]
    async fn test_brave_search_live() {
        let html = cached_html(
            "brave/search_p0.html",
            &build_search_url("rust async", 0),
            looks_like_search_results,
        )
        .await;
        let results = parse_search_response(&html).unwrap();
        assert!(!results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn test_brave_search_pagination_live() {
        let page1_html = cached_html(
            "brave/search_p0.html",
            &build_search_url("rust async", 0),
            looks_like_search_results,
        )
        .await;
        let page2_html = cached_html(
            "brave/search_p1.html",
            &build_search_url("rust async", BRAVE_RESULTS_PER_PAGE),
            looks_like_search_results,
        )
        .await;

        let page1 = parse_search_response(&page1_html).unwrap();
        let page2 = parse_search_response(&page2_html).unwrap();

        assert!(!page1.is_empty());
        assert!(!page2.is_empty());
        assert!(
            page1
                .iter()
                .all(|r| !page2.iter().any(|r2| r2.url == r.url)),
            "page 2 should not repeat page 1's results"
        );
    }

    #[ignore]
    #[tokio::test]
    async fn test_brave_images_live() {
        let html = cached_html(
            "brave/images_p0.html",
            &build_image_search_url("rust async"),
            looks_like_image_results,
        )
        .await;
        let images = parse_image_response(&html).unwrap();
        assert!(!images.is_empty());
    }
}
