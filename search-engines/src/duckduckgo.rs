use async_trait::async_trait;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode, utf8_percent_encode};
use reqwest::Url;

use crate::{
    BlockKind, EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client,
    parse_search,
};

#[derive(Clone)]
pub struct DuckDuckGo;

impl EngineInfo for DuckDuckGo {
    fn name(&self) -> &'static str {
        "DuckDuckGo"
    }
}

// `s`/`dc` mirror DDG's own "More results" link params; unofficial, may need
// revalidating against `test_duckduckgo_pagination_live` if DDG's markup changes.
fn build_search_url(query: &str, start: usize) -> String {
    let query = utf8_percent_encode(query, NON_ALPHANUMERIC);
    let mut url = format!("https://html.duckduckgo.com/html?q={query}");
    if start > 0 {
        url.push_str(&format!("&s={start}&dc={}", start + 1));
    }
    url
}

// A real DDG results page always has this wrapper, even with 0 hits; the
// bot-wall "anomaly"/captcha interstitial DDG serves instead has completely
// different markup. Without this check a block silently parses to an empty
// Vec, indistinguishable from genuine exhaustion.
fn looks_like_search_results(html: &str) -> bool {
    html.contains("serp__results")
}

fn looks_like_captcha(html: &str) -> bool {
    html.contains(r#"id="challenge-form""#) || html.contains("anomaly")
}

#[async_trait]
impl SearchEngine for DuckDuckGo {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let resp = browser_client()
            .get(build_search_url(query, start))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        let html = body_or_block(resp, "DuckDuckGo").await?;
        if looks_like_captcha(&html) {
            // DDG serves its challenge page with HTTP 202, so status alone
            // cannot detect this bot wall.
            return Err(EngineError::Blocked {
                kind: BlockKind::Captcha,
                retry_after: None,
                detail: "DuckDuckGo challenge page".into(),
            });
        }
        if !looks_like_search_results(&html) {
            return Err(EngineError::ParseError(
                "DuckDuckGo response markup may have changed; results marker was missing".into(),
            ));
        }

        parse_response(&html)
    }
}

pub fn parse_response(html: &str) -> Result<Vec<RawResult>, EngineError> {
    // Both organic and sponsored results are wrapped in the same
    // `duckduckgo.com/l/?uddg=` click-tracking redirect these days, so a
    // result's href can't tell ads apart from organic hits anymore — DDG
    // marks ads on the *result container* itself via a `result--ad` class
    // instead, so exclude those at the selector level.
    let results = parse_search(
        html,
        ".serp__results .result:not(.result--ad)",
        ".result__a",
        ".result__a",
        ".result__snippet",
    )
    .into_iter()
    .map(|mut r| {
        r.url = extract_ddg_url(&r.url).unwrap();
        r
    })
    .collect();

    Ok(results)
}

fn extract_ddg_url(ddg_href: &str) -> Option<String> {
    // Decode the DDG redirect link
    let url = Url::parse("https://duckduckgo.com")
        .ok()?
        .join(ddg_href)
        .ok()?;
    for (k, v) in url.query_pairs() {
        if k == "uddg" {
            return Some(percent_decode(v.as_bytes()).decode_utf8().ok()?.to_string());
        }
    }
    Some(ddg_href.to_string()) // fallback to raw href
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn build_search_url_omits_pagination_params_on_first_page() {
        assert_eq!(
            build_search_url("rust async", 0),
            "https://html.duckduckgo.com/html?q=rust%20async"
        );
    }

    #[test]
    fn build_search_url_adds_start_and_dc_for_later_pages() {
        assert_eq!(
            build_search_url("rust async", 20),
            "https://html.duckduckgo.com/html?q=rust%20async&s=20&dc=21"
        );
    }

    #[test]
    fn build_search_url_encodes_reserved_characters() {
        assert_eq!(
            build_search_url("AT&T c++ #tag", 0),
            "https://html.duckduckgo.com/html?q=AT%26T%20c%2B%2B%20%23tag"
        );
    }

    /// Non-ASCII queries must survive a full percent-encode round trip —
    /// regression guard for "encoding support" (unicode search terms are a
    /// normal, not edge-case, input for a search engine).
    #[test]
    fn build_search_url_encodes_non_ascii_query() {
        assert_eq!(
            build_search_url("café 日本語", 0),
            "https://html.duckduckgo.com/html?q=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E"
        );
    }

    #[test]
    fn looks_like_search_results_rejects_a_block_page() {
        assert!(looks_like_search_results(
            r#"<div class="serp__results">...</div>"#
        ));
        assert!(!looks_like_search_results(
            "<html><body>unusual traffic</body></html>"
        ));
    }

    #[test]
    fn looks_like_captcha_detects_the_ddg_challenge_page() {
        assert!(looks_like_captcha(
            r#"<form id="challenge-form">captcha</form>"#
        ));
        assert!(!looks_like_captcha(SEARCH_FIXTURE));
    }

    #[test]
    fn extract_ddg_url_decodes_redirect_param() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc123";
        assert_eq!(
            extract_ddg_url(href),
            Some("https://example.com/rust".to_string())
        );
    }

    #[test]
    fn extract_ddg_url_falls_back_to_raw_href_without_uddg() {
        let href = "https://example.com/no-redirect";
        assert_eq!(extract_ddg_url(href), Some(href.to_string()));
    }

    // Mirrors DDG's real current markup (confirmed against a live fetch):
    // organic *and* sponsored links both go through the same
    // `duckduckgo.com/l/?uddg=` tracking redirect, so only the result
    // container's `result--ad` class distinguishes an ad from an organic hit.
    const SEARCH_FIXTURE: &str = r#"
        <div class="serp__results">
            <div class="result results_links results_links_deep web-result ">
                <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">Rust Programming Language</a>
                <a class="result__snippet">A systems programming language.</a>
            </div>
            <div class="result results_links results_links_deep result--ad ">
                <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fad&rut=xyz">Sponsored: Learn Rust Fast</a>
                <a class="result__snippet">Sponsored description.</a>
            </div>
        </div>
    "#;

    #[test]
    fn parse_response_filters_sponsored_results() {
        let results = parse_response(SEARCH_FIXTURE).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].description, "A systems programming language.");
    }

    use crate::fixtures::cached_html;

    // DDG bot-walls datacenter IPs with an "anomaly" page; retry from a
    // residential connection if the first-run fetch fails here.
    #[ignore]
    #[tokio::test]
    async fn test_duckduckgo_live() {
        let html = cached_html(
            "duckduckgo/search_p0.html",
            &build_search_url("rust async", 0),
            looks_like_search_results,
        )
        .await;
        let results = parse_response(&html).unwrap();
        assert!(!results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn test_duckduckgo_pagination_live() {
        let page1_html = cached_html(
            "duckduckgo/search_p0.html",
            &build_search_url("rust async", 0),
            looks_like_search_results,
        )
        .await;
        let page2_html = cached_html(
            "duckduckgo/search_p1.html",
            &build_search_url("rust async", 20),
            looks_like_search_results,
        )
        .await;

        let page1 = parse_response(&page1_html).unwrap();
        let page2 = parse_response(&page2_html).unwrap();

        assert!(!page1.is_empty());
        assert!(!page2.is_empty());
        assert!(
            page1
                .iter()
                .all(|r| !page2.iter().any(|r2| r2.url == r.url)),
            "page 2 should not repeat page 1's results — if this fails, DDG's \
             `s`/`dc` pagination params may need revisiting"
        );
    }
}
