use async_trait::async_trait;
use percent_encoding::percent_decode;
use reqwest::Url;
use scraper::{Html, Selector};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::{
    BlockKind, EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client,
    parse_search,
};

#[derive(Clone)]
pub struct DuckDuckGo;

const POST_URL: &str = "https://html.duckduckgo.com/html/";
const VQD_TTL: Duration = Duration::from_secs(60 * 60);
// Keep tokens in this bounded, process-local map because `search-engines` is
// the pure adapter layer and must not depend on the project's SQLite cache.
static VQD_CACHE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();

impl EngineInfo for DuckDuckGo {
    fn name(&self) -> &'static str {
        "DuckDuckGo"
    }
}

fn extract_vqd(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("input[name=\"vqd\"]").ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|input| input.value().attr("value"))
        .map(str::to_owned)
}

fn cached_vqd(query: &str) -> Option<String> {
    let cache = VQD_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().ok()?;
    let now = Instant::now();
    let (vqd, stored_at) = cache
        .get_key_value(query)
        .map(|(_, (vqd, stored_at))| (vqd.clone(), *stored_at))?;
    if now.duration_since(stored_at) >= VQD_TTL {
        cache.remove(query);
        return None;
    }
    Some(vqd)
}

fn store_vqd(query: &str, vqd: String) {
    let cache = VQD_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut cache) = cache.lock() else {
        return;
    };
    let now = Instant::now();
    cache.retain(|_, (_, stored_at)| now.duration_since(*stored_at) < VQD_TTL);
    cache.insert(query.to_owned(), (vqd, now));
}

fn build_form(query: &str, start: usize, vqd: Option<&str>) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("q", query.to_owned()),
        ("b", String::new()),
        ("kl", "wt-wt".to_owned()),
    ];
    if start > 0 {
        form.extend([("s", start.to_string()), ("dc", (start + 1).to_string())]);
        if let Some(vqd) = vqd {
            form.push(("vqd", vqd.to_owned()));
        }
    }
    form
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
        search_results_at(POST_URL, query, start).await
    }
}

async fn search_results_at(
    base_url: &str,
    query: &str,
    start: usize,
) -> Result<Vec<RawResult>, EngineError> {
    let vqd = if start > 0 {
        let Some(vqd) = cached_vqd(query) else {
            // DDG reads a paginated request without a `vqd` as bot traffic.
            return Err(EngineError::Blocked {
                kind: BlockKind::Captcha,
                retry_after: None,
                detail: "DuckDuckGo pagination requires a vqd token".into(),
            });
        };
        Some(vqd)
    } else {
        None
    };

    let resp = browser_client()
        .post(base_url)
        .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
        .form(&build_form(query, start, vqd.as_deref()))
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
    if start == 0
        && let Some(vqd) = extract_vqd(&html)
    {
        // This adapter has no cache dependency: a process-local token is
        // enough while keeping `search-engines` pure and cache-free.
        store_vqd(query, vqd);
    }

    parse_response(&html)
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
    fn extract_vqd_reads_the_hidden_form_input() {
        let html = r#"<input type="hidden" name="vqd" value="4-123abc">"#;

        assert_eq!(extract_vqd(html), Some("4-123abc".to_owned()));
    }

    #[test]
    fn extract_vqd_returns_none_when_absent() {
        assert_eq!(extract_vqd(SEARCH_FIXTURE), None);
    }

    #[test]
    fn a_stored_vqd_is_returned_for_the_same_query() {
        let query = "vqd-cache-same-query-008";
        store_vqd(query, "4-same-query".to_owned());

        assert_eq!(cached_vqd(query), Some("4-same-query".to_owned()));
    }

    #[test]
    fn a_vqd_for_a_different_query_is_not_returned() {
        store_vqd("vqd-cache-source-query-008", "4-source-query".to_owned());

        assert_eq!(cached_vqd("vqd-cache-different-query-008"), None);
    }

    #[test]
    fn build_form_omits_pagination_fields_on_first_page() {
        let form = build_form("rust async", 0, None);

        assert_eq!(
            form,
            vec![
                ("q", "rust async".to_owned()),
                ("b", String::new()),
                ("kl", "wt-wt".to_owned()),
            ]
        );
    }

    #[test]
    fn build_form_adds_pagination_fields_for_later_pages() {
        assert_eq!(
            build_form("rust async", 20, Some("4-123abc")),
            vec![
                ("q", "rust async".to_owned()),
                ("b", String::new()),
                ("kl", "wt-wt".to_owned()),
                ("s", "20".to_owned()),
                ("dc", "21".to_owned()),
                ("vqd", "4-123abc".to_owned()),
            ]
        );
    }

    #[test]
    fn build_form_carries_reserved_characters_verbatim() {
        let form = build_form("AT&T c++ #tag", 0, None);

        assert_eq!(form[0], ("q", "AT&T c++ #tag".to_owned()));
    }

    #[test]
    fn build_form_carries_non_ascii_query_verbatim() {
        let form = build_form("café 日本語", 0, None);

        assert_eq!(form[0], ("q", "café 日本語".to_owned()));
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

    #[tokio::test]
    async fn page_one_search_posts_form_and_referer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            let header_end = loop {
                let bytes_read = stream.read(&mut chunk).await.unwrap();
                assert!(bytes_read > 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..bytes_read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break header_end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .or_else(|| line.strip_prefix("content-length:"))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("form POST should include Content-Length");
            while request.len() - header_end < content_length {
                let bytes_read = stream.read(&mut chunk).await.unwrap();
                assert!(bytes_read > 0, "request ended before its form body");
                request.extend_from_slice(&chunk[..bytes_read]);
            }

            let response_body = format!(
                "<input type=\"hidden\" name=\"vqd\" value=\"4-localhost\">{SEARCH_FIXTURE}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        search_results_at(&format!("http://{address}/"), "post shape 008", 0)
            .await
            .unwrap();

        let request = server.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST / HTTP/1.1"));
        assert!(body.contains("q="));
        assert!(body.contains("kl=wt-wt"));
        assert!(head.to_ascii_lowercase().contains("referer:"));
    }

    #[tokio::test]
    async fn paginating_without_a_vqd_refuses_to_send_a_request() {
        let result = search_results_at("http://127.0.0.1:1/", "no-vqd-pagination-008", 20).await;

        assert!(matches!(
            result,
            Err(EngineError::Blocked {
                kind: BlockKind::Captcha,
                ..
            })
        ));
    }

    // DDG bot-walls datacenter IPs with an "anomaly" page; retry from a
    // residential connection if a manually enabled live test fails here.
    #[ignore]
    #[tokio::test]
    async fn test_duckduckgo_live() {
        let results = DuckDuckGo
            .search_results("rust async", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn test_duckduckgo_pagination_live() {
        let page1 = DuckDuckGo
            .search_results("rust async", 0, 20)
            .await
            .unwrap();
        let page2 = DuckDuckGo
            .search_results("rust async", 20, 20)
            .await
            .unwrap();

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
