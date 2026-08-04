use crate::{
    cache::{ImagesRow, ResultRow},
    engines::{
        EngineError, EngineInfo, ImageEngine, SearchEngine, new_rand_client, parse_images,
        parse_search,
    },
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

// A real Brave results page always has this container, even with 0 hits;
// only a block/captcha interstitial omits it. Without this check a block
// silently parses to an empty Vec, indistinguishable from genuine exhaustion.
fn looks_like_search_results(html: &str) -> bool {
    html.contains(r#"id="results""#)
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
    ) -> Result<Vec<ResultRow>, EngineError> {
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

pub fn parse_search_response(html: &str) -> Result<Vec<ResultRow>, EngineError> {
    Ok(parse_search(
        html,
        "#results > .snippet[data-pos]:not(.standalone)",
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
    ) -> Result<Vec<ImagesRow>, EngineError> {
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

pub fn parse_image_response(html: &str) -> Result<Vec<ImagesRow>, EngineError> {
    Ok(parse_images(
        html,
        ".image-result",
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

    #[test]
    fn looks_like_search_results_rejects_a_block_page() {
        assert!(looks_like_search_results(r#"<div id="results">...</div>"#));
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

    const SEARCH_FIXTURE: &str = r#"
        <div id="results">
            <div class="snippet" data-pos="1">
                <a href="https://example.com/rust">
                    <div class="title">Rust Programming Language</div>
                </a>
                <div class="generic-snippet">A systems language for reliable software.</div>
            </div>
            <div class="snippet" data-pos="2">
                <a href="https://example.com/video">
                    <div class="title">Rust in 100 seconds</div>
                </a>
                <div class="video-snippet">
                    <div class="snippet-description">A quick video overview.</div>
                </div>
            </div>
            <div class="snippet standalone" data-pos="3">
                <a href="https://example.com/ignored">
                    <div class="title">Should be excluded</div>
                </a>
                <div class="generic-snippet">Standalone snippets aren't real results.</div>
            </div>
        </div>
    "#;

    #[test]
    fn parse_search_response_extracts_results_and_skips_standalone() {
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

        assert!(results.iter().all(|r| r.url != "https://example.com/ignored"));
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

    use crate::engines::fixtures::cached_html;

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
            page1.iter().all(|r| !page2.iter().any(|r2| r2.url == r.url)),
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
