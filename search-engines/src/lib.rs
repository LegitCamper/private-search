//! Pure, cache-free adapters for scraping privacy-respecting search engines.
//!
//! This crate knows nothing about caching or ranking — it only knows how to
//! turn `(query, start)` into a page of raw results for a given engine.
//! Pair it with a cache/merge layer (e.g. `search-cache`) to get pagination,
//! deduplication, and persistence.

use async_trait::async_trait;
use reqwest::{
    Client,
    header::{
        ACCEPT, ACCEPT_LANGUAGE, CONNECTION, DNT, HeaderMap, HeaderName, HeaderValue,
        UPGRADE_INSECURE_REQUESTS,
    },
};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::time::Duration;

mod brave;
mod duckduckgo;

pub use brave::Brave;
pub use duckduckgo::DuckDuckGo;

/// One raw text-search hit, straight off an engine's results page — no
/// ranking, dedup, or engine attribution applied yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawResult {
    pub url: String,
    pub title: String,
    pub description: String,
}

/// One raw image-search hit, straight off an engine's results page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawImage {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    RateLimited,
    AccessDenied,
    Captcha,
    ServerError,
}

impl std::fmt::Display for BlockKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            BlockKind::RateLimited => "rate limited",
            BlockKind::AccessDenied => "access denied",
            BlockKind::Captcha => "captcha",
            BlockKind::ServerError => "server error",
        };
        f.write_str(name)
    }
}

#[derive(Debug)]
pub enum EngineError {
    ReqwestError(reqwest::Error),
    ParseError(String),
    Timeout, // engine timeout
    // A captcha or rate limit calls for backoff, while a missing results
    // marker on a healthy 200 means our selectors are stale and need a code
    // fix. Keeping those cases distinct makes both diagnosable.
    Blocked {
        kind: BlockKind,
        retry_after: Option<Duration>,
        detail: String,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::ReqwestError(e) => write!(f, "request failed: {e}"),
            EngineError::ParseError(e) => write!(f, "parse error: {e}"),
            EngineError::Timeout => write!(f, "engine timed out"),
            EngineError::Blocked {
                kind,
                retry_after,
                detail,
            } => {
                write!(f, "engine {kind}: {detail}")?;
                if let Some(retry_after) = retry_after {
                    write!(f, " (retry after {}s)", retry_after.as_secs())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for EngineError {}

#[async_trait]
pub trait EngineInfo: Clone + Send {
    fn name(&self) -> &'static str;
}

#[async_trait]
pub trait SearchEngine: EngineInfo + Clone + Send {
    /// Fetches one page of results. `start` is how many the caller already
    /// has; `count` is a hint some engines can't honor exactly. An empty
    /// `Vec` signals no more results.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<RawResult>, EngineError>;
}

#[async_trait]
pub trait ImageEngine: EngineInfo + Clone + Send {
    /// See [`SearchEngine::search_results`] — same paging contract.
    async fn search_images(
        &self,
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<RawImage>, EngineError>;
}

/// A single Firefox identity keeps the complete header set internally
/// consistent across requests. Firefox is used because it does not send
/// Chrome's client-hint headers, which this client cannot plausibly emulate.
static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();

/// Builds one reusable browser-like client so its identity stays stable for
/// token-based flows while requests benefit from connection pooling.
fn browser_client() -> Client {
    CLIENT
        .get_or_init(|| {
            let mut headers = HeaderMap::new();
            headers.insert(
                ACCEPT,
                HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ),
            );
            headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.5"));
            headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
            headers.insert(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"));
            headers.insert(DNT, HeaderValue::from_static("1"));
            headers.insert(
                HeaderName::from_static("sec-fetch-dest"),
                HeaderValue::from_static("document"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-mode"),
                HeaderValue::from_static("navigate"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-site"),
                HeaderValue::from_static("none"),
            );
            headers.insert(
                HeaderName::from_static("sec-fetch-user"),
                HeaderValue::from_static("?1"),
            );

            Client::builder()
                .user_agent(
                    "Mozilla/5.0 (X11; Linux x86_64; rv:153.0) Gecko/20100101 Firefox/153.0",
                )
                .default_headers(headers)
                .build()
                .expect("failed to build browser client")
        })
        .clone()
}

fn classify_status(
    status: u16,
    retry_after: Option<&str>,
) -> Option<(BlockKind, Option<Duration>)> {
    let retry_after = retry_after
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);

    match status {
        402 | 429 => Some((BlockKind::RateLimited, retry_after)),
        403 => Some((BlockKind::AccessDenied, None)),
        500..=599 => Some((BlockKind::ServerError, None)),
        _ => None,
    }
}

pub(crate) async fn body_or_block(
    resp: reqwest::Response,
    engine: &'static str,
) -> Result<String, EngineError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    if let Some((kind, retry_after)) = classify_status(status.as_u16(), retry_after.as_deref()) {
        return Err(EngineError::Blocked {
            kind,
            retry_after,
            detail: format!("{engine} returned HTTP {}", status.as_u16()),
        });
    }

    resp.text().await.map_err(EngineError::ReqwestError)
}

/// Record-once, replay-forever HTML fixtures for the "live" engine tests: the
/// first run hits the real engine and saves the response under
/// `tests/fixtures/`; every run after that reparses the file with no network
/// call, so rerunning tests can't get an IP banned. Commit the fixtures so CI
/// needs no network either. To force a fresh capture, delete the fixture file
/// or set `REFRESH_LIVE_FIXTURES=1`.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::{Path, PathBuf};

    fn path_for(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(relative)
    }

    /// `relative` is a distinct key per page, e.g. `"brave/search_p0.html"` vs
    /// `"brave/search_p1.html"`, so pagination tests don't collide.
    /// `looks_valid` is checked before writing to disk — engines like DDG
    /// serve a captcha page as a normal 200, and without this it would get
    /// cached forever as if it were real data, e.g. `|h| h.contains("serp__results")`.
    pub(crate) async fn cached_html(
        relative: &str,
        url: &str,
        looks_valid: impl Fn(&str) -> bool,
    ) -> String {
        let path = path_for(relative);
        let refresh = std::env::var_os("REFRESH_LIVE_FIXTURES").is_some();

        if !refresh && let Ok(html) = std::fs::read_to_string(&path) {
            return html;
        }

        let html = super::browser_client()
            .get(url)
            .send()
            .await
            .unwrap_or_else(|e| panic!("live fetch of {url} failed: {e}"))
            .text()
            .await
            .expect("failed to read response body");

        assert!(
            looks_valid(&html),
            "live fetch of {url} didn't look like a real response (bot wall / \
             captcha page?) — refusing to cache it. Try again from a different \
             network."
        );

        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create fixtures dir");
        std::fs::write(&path, &html).expect("failed to write fixture");

        html
    }
}

const PARSE_ERROR: &str = "Couldnt parse selector string";

pub fn parse_search(
    html: &str,
    results_selector: &'static str,
    title_selector: &'static str,
    href_selector: &'static str,
    description_selector: &'static str,
) -> Vec<RawResult> {
    let html = Html::parse_document(html);

    let results_selector = Selector::parse(results_selector).expect(PARSE_ERROR);
    let title_selector = Selector::parse(title_selector).expect(PARSE_ERROR);
    let href_selector = Selector::parse(href_selector).expect(PARSE_ERROR);
    let description_selector = Selector::parse(description_selector).expect(PARSE_ERROR);

    let mut results = Vec::new();

    for result in html.select(&results_selector) {
        results.push(RawResult {
            url: result
                .select(&href_selector)
                .next()
                .and_then(|u| u.value().attr("href"))
                .unwrap_or_default()
                .to_string(),

            title: result
                .select(&title_selector)
                .next()
                .map(|t| t.text().collect::<String>())
                .unwrap_or_default(),

            description: result
                .select(&description_selector)
                .next()
                .map(|d| d.text().collect::<String>())
                .unwrap_or_default(),
        })
    }

    results
}

pub fn parse_images(
    html: &str,
    images_selector: &'static str,
    title_selector: &'static str,
    img_selector: &'static str,
) -> Vec<RawImage> {
    let html = Html::parse_document(html);

    let images_selector = Selector::parse(images_selector).expect(PARSE_ERROR);
    let title_selector = Selector::parse(title_selector).expect(PARSE_ERROR);
    let img_selector = Selector::parse(img_selector).expect(PARSE_ERROR);

    let mut images = Vec::new();

    for result in html.select(&images_selector) {
        images.push(RawImage {
            url: result
                .select(&img_selector)
                .next()
                .and_then(|u| u.value().attr("src"))
                .unwrap_or_default()
                .to_string(),

            title: result
                .select(&title_selector)
                .next()
                .map(|t| t.text().collect::<String>())
                .unwrap_or_default(),
        })
    }

    images
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn classify_status_maps_429_to_rate_limited() {
        assert_eq!(
            classify_status(429, None),
            Some((BlockKind::RateLimited, None))
        );
    }

    #[test]
    fn classify_status_parses_retry_after_seconds() {
        assert_eq!(
            classify_status(429, Some("120")),
            Some((BlockKind::RateLimited, Some(Duration::from_secs(120))))
        );
    }

    #[test]
    fn classify_status_maps_403_to_access_denied() {
        assert_eq!(
            classify_status(403, Some("120")),
            Some((BlockKind::AccessDenied, None))
        );
    }

    #[test]
    fn classify_status_maps_5xx_to_server_error() {
        assert_eq!(
            classify_status(503, None),
            Some((BlockKind::ServerError, None))
        );
    }

    #[test]
    fn classify_status_returns_none_for_200_and_202() {
        assert_eq!(classify_status(200, None), None);
        assert_eq!(classify_status(202, None), None);
    }

    #[test]
    fn parse_search_extracts_all_fields() {
        let html = r#"
            <div class="result">
                <a class="title" href="https://example.com/page">Example Title</a>
                <p class="desc">Example description</p>
            </div>
        "#;

        let results = parse_search(html, ".result", ".title", ".title", ".desc");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/page");
        assert_eq!(results[0].title, "Example Title");
        assert_eq!(results[0].description, "Example description");
    }

    #[test]
    fn parse_search_defaults_missing_fields_to_empty_string() {
        let html = r#"<div class="result"></div>"#;

        let results = parse_search(html, ".result", ".title", ".title", ".desc");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "");
        assert_eq!(results[0].title, "");
        assert_eq!(results[0].description, "");
    }

    #[test]
    fn parse_search_returns_empty_vec_when_selector_matches_nothing() {
        let html = "<div>no results here</div>";

        let results = parse_search(html, ".result", ".title", ".title", ".desc");

        assert!(results.is_empty());
    }

    #[test]
    fn parse_images_extracts_url_and_title() {
        let html = r#"
            <div class="image">
                <img src="https://example.com/pic.png">
                <span class="caption">A picture</span>
            </div>
        "#;

        let images = parse_images(html, ".image", ".caption", "img");

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].url, "https://example.com/pic.png");
        assert_eq!(images[0].title, "A picture");
    }

    #[test]
    fn parse_images_defaults_missing_fields_to_empty_string() {
        let html = r#"<div class="image"></div>"#;

        let images = parse_images(html, ".image", ".caption", "img");

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].url, "");
        assert_eq!(images[0].title, "");
    }

    #[tokio::test]
    async fn browser_client_sends_consistent_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let bytes_read = stream.read(&mut chunk).await.unwrap();
                assert!(bytes_read > 0, "request ended before its header terminator");
                request.extend_from_slice(&chunk[..bytes_read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let response = browser_client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "hi");

        let request = server.await.unwrap();
        let request_lowercase = request.to_ascii_lowercase();
        let user_agent = request_lowercase
            .lines()
            .find(|line| line.starts_with("user-agent:"))
            .expect("the browser client should send a User-Agent header");
        assert!(user_agent.contains("firefox/153.0"));
        assert!(request_lowercase.contains("accept:"));
        assert!(request_lowercase.contains("accept-language:"));
        assert!(request_lowercase.contains("sec-fetch-mode:"));
        assert!(request_lowercase.contains("upgrade-insecure-requests:"));

        let accept_encoding = request_lowercase
            .lines()
            .find(|line| line.starts_with("accept-encoding:"));
        println!("reqwest sent Accept-Encoding: {accept_encoding:?}");
    }
}
