use async_trait::async_trait;
use reqwest::Url;
use scraper::{Html, Selector};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::{
    BlockKind, EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, parse_search,
    proxy::{self, Plan, Route, RouteId},
};

#[derive(Clone)]
pub struct DuckDuckGo;

const POST_URL: &str = "https://html.duckduckgo.com/html/";
const CONTINUATION_TTL: Duration = Duration::from_secs(60 * 60);
type Form = Vec<(String, String)>;
#[derive(Clone)]
struct Continuation {
    form: Option<Form>,
    route: RouteId,
    stored_at: Instant,
}
type Continuations = HashMap<(String, usize), Continuation>;
static CONTINUATIONS: OnceLock<Mutex<Continuations>> = OnceLock::new();

impl EngineInfo for DuckDuckGo {
    fn name(&self) -> &'static str {
        "DuckDuckGo"
    }
}

fn extract_next_form(html: &str, current_offset: usize) -> Option<Form> {
    let document = Html::parse_document(html);
    let forms = Selector::parse("form").ok()?;
    let inputs = Selector::parse("input[type=hidden][name]").ok()?;
    document
        .select(&forms)
        .map(|form| {
            form.select(&inputs)
                .filter_map(|input| {
                    Some((
                        input.value().attr("name")?.to_owned(),
                        input.value().attr("value").unwrap_or_default().to_owned(),
                    ))
                })
                .collect::<Form>()
        })
        .find(|form| {
            form_offset(form).is_some_and(|offset| offset > current_offset)
                && form
                    .iter()
                    .any(|(name, value)| name == "vqd" && !value.is_empty())
        })
}

fn form_offset(form: &Form) -> Option<usize> {
    form.iter()
        .find(|(name, _)| name == "s")
        .and_then(|(_, value)| value.parse().ok())
}

fn cached_continuation(query: &str, start: usize) -> Option<Continuation> {
    let cache = CONTINUATIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().ok()?;
    let key = (query.to_owned(), start);
    let entry = cache.get(&key)?;
    if entry.stored_at.elapsed() >= CONTINUATION_TTL {
        cache.remove(&key);
        return None;
    }
    Some(entry.clone())
}

fn store_continuation(query: &str, start: usize, form: Option<Form>, route: RouteId) {
    let cache = CONTINUATIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut cache) = cache.lock() else {
        return;
    };
    let now = Instant::now();
    cache.retain(|_, entry| now.duration_since(entry.stored_at) < CONTINUATION_TTL);
    if cache.len() >= 512
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.stored_at)
            .map(|(key, _)| key.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(
        (query.to_owned(), start),
        Continuation {
            form,
            route,
            stored_at: now,
        },
    );
}

fn build_form(query: &str) -> Form {
    vec![
        ("q".into(), query.to_owned()),
        ("b".into(), String::new()),
        ("kl".into(), "wt-wt".to_owned()),
    ]
}

// A real DDG results page always has this wrapper, even with 0 hits; the
// bot-wall "anomaly"/captcha interstitial DDG serves instead has completely
// different markup. Without this check a block silently parses to an empty
// Vec, indistinguishable from genuine exhaustion.
fn looks_like_search_results(html: &str) -> bool {
    html.contains("serp__results")
}

// Matched against the challenge page's own markup, never a bare word: DDG's
// bot wall renders `anomaly-modal__*` containers and a `challenge-form`, while
// the word "anomaly" on its own appears in perfectly ordinary result titles
// and snippets ("anomaly detection"), which used to burn the engine for the
// whole cooldown window.
fn looks_like_captcha(html: &str) -> bool {
    let document = Html::parse_document(html);
    let selector = Selector::parse("form#challenge-form, .anomaly-modal, .anomaly-modal__modal")
        .expect("static challenge selector");
    document.select(&selector).next().is_some()
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
    let (form, pinned) = if start == 0 {
        (build_form(query), None)
    } else {
        let continuation = cached_continuation(query, start).ok_or_else(|| {
            EngineError::PaginationExpired("DuckDuckGo continuation is unavailable".into())
        })?;
        let Some(form) = continuation.form else {
            return Ok(Vec::new());
        };
        (form, Some(continuation.route))
    };

    if pinned.as_ref().is_some_and(|route| {
        proxy::enabled_for("DuckDuckGo") && proxy::pinned_route(route, "DuckDuckGo").is_none()
    }) {
        return Err(EngineError::PaginationExpired(
            "DuckDuckGo pinned proxy is unavailable".into(),
        ));
    }

    let won = proxy::race(
        Plan {
            engine: "DuckDuckGo",
            target: base_url,
            pinned,
        },
        |route| fetch_results(route, base_url, &form),
    )
    .await?;

    let (results, next_form) = won.value;
    if !results.is_empty() {
        store_continuation(
            query,
            start.saturating_add(results.len()),
            next_form,
            won.route,
        );
    }
    Ok(results)
}

async fn fetch_results(
    route: Route,
    base_url: &str,
    form: &Form,
) -> Result<(Vec<RawResult>, Option<Form>), EngineError> {
    let resp = route
        .client()
        .post(base_url)
        .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
        .form(form)
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
        return Err(proxy::marker_error(
            "DuckDuckGo",
            &route.id,
            "DuckDuckGo response markup may have changed; results marker was missing",
        ));
    }

    Ok((
        parse_response(&html)?,
        extract_next_form(&html, form_offset(form).unwrap_or(0)),
    ))
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
    .filter_map(|mut r| {
        // A result whose redirect target can't be recovered is dropped, not
        // fatal: one malformed href used to panic the whole engine task.
        r.url = extract_ddg_url(&r.url)?;
        Some(r)
    })
    .collect();

    Ok(results)
}

fn extract_ddg_url(ddg_href: &str) -> Option<String> {
    // Decode the DDG redirect link. `query_pairs` already percent-decodes the
    // value, so decoding it a second time would corrupt destinations that
    // legitimately contain encoded characters (`%252F` -> `/`, `%2526` -> `&`).
    let url = Url::parse("https://duckduckgo.com")
        .ok()?
        .join(ddg_href)
        .ok()?;
    let target = url
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.to_string());
    let candidate = match target {
        Some(target) => target,
        // Not a redirect wrapper: keep the href as-is.
        None => url.to_string(),
    };
    // Scraped markup is untrusted; only ever emit http(s) destinations.
    let parsed = Url::parse(&candidate).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(candidate)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn ordinary_search_text_cannot_trigger_a_challenge() {
        assert!(!looks_like_captcha(
            r#"<div class="serp__results"><a>anomaly detection</a><p>anomaly-modal docs</p></div>"#
        ));
        assert!(looks_like_captcha("<form id='challenge-form'></form>"));
    }

    #[test]
    fn redirect_decoding_preserves_destination_escapes_and_rejects_scripts() {
        assert_eq!(
            extract_ddg_url("/l/?uddg=https%3A%2F%2Fexample.com%2Fa%252Fb%3Fx%3D%2526"),
            Some("https://example.com/a%2Fb?x=%26".into())
        );
        assert!(extract_ddg_url("/l/?uddg=javascript%3Aalert(1)").is_none());
        assert!(extract_ddg_url("http://[").is_none());
    }

    #[test]
    fn continuation_preserves_all_fields_and_skips_previous_page() {
        let html = format!("{}{}", next_form(0, "old"), next_form(50, "new"));
        let form = extract_next_form(&html, 30).unwrap();
        assert_eq!(form_offset(&form), Some(50));
        assert!(form.contains(&("vqd".into(), "new".into())));
        assert!(form.contains(&("api".into(), "d.js".into())));
        assert!(form.contains(&("nextParams".into(), "a&b".into())));
        assert!(extract_next_form(&html, 50).is_none());
    }

    #[test]
    fn continuation_returns_none_when_absent() {
        assert_eq!(extract_next_form(SEARCH_FIXTURE, 0), None);
    }

    #[test]
    fn continuation_is_scoped_to_query_and_raw_offset() {
        let query = "continuation-cache-query";
        let form = extract_next_form(&next_form(30, "token"), 0);
        store_continuation(query, 19, form.clone(), RouteId::Direct);
        let cached = cached_continuation(query, 19).unwrap();
        assert_eq!(cached.form, form);
        assert_eq!(cached.route, RouteId::Direct);
        assert!(cached_continuation(query, 20).is_none());
        assert!(cached_continuation("another-query", 19).is_none());
    }

    #[test]
    fn build_form_omits_pagination_fields_on_first_page() {
        let form = build_form("rust async");

        assert_eq!(
            form,
            vec![
                ("q".into(), "rust async".to_owned()),
                ("b".into(), String::new()),
                ("kl".into(), "wt-wt".to_owned()),
            ]
        );
    }

    #[test]
    fn build_form_carries_reserved_characters_verbatim() {
        let form = build_form("AT&T c++ #tag");

        assert_eq!(form[0], ("q".into(), "AT&T c++ #tag".to_owned()));
    }

    #[test]
    fn build_form_carries_non_ascii_query_verbatim() {
        let form = build_form("café 日本語");

        assert_eq!(form[0], ("q".into(), "café 日本語".to_owned()));
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
    async fn missing_continuation_is_not_an_engine_block() {
        let result = search_results_at("http://127.0.0.1:1/", "no-vqd-pagination-008", 20).await;

        assert!(matches!(result, Err(EngineError::PaginationExpired(_))));
    }

    fn next_form(offset: usize, token: &str) -> String {
        format!(
            r#"<form method="post">
            <input type="hidden" name="q" value="paging-test">
            <input type="hidden" name="s" value="{offset}">
            <input type="hidden" name="vqd" value="{token}">
            <input type="hidden" name="api" value="d.js">
            <input type="hidden" name="nextParams" value="a&amp;b">
        </form>"#
        )
    }

    #[tokio::test]
    async fn pagination_replays_forms_rotates_tokens_and_keeps_state_after_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, form) in [
                (200, next_form(30, "first")),
                (500, String::new()),
                (200, next_form(50, "second")),
                (200, String::new()),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let length = stream.read(&mut chunk).await.unwrap();
                    assert!(length > 0);
                    request.extend_from_slice(&chunk[..length]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            requests.push(String::from_utf8(request[end + 4..].to_vec()).unwrap());
                            break;
                        }
                    }
                }
                let body = format!("{SEARCH_FIXTURE}{form}");
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let base = format!("http://{address}/");
        let query = "paging-test";
        assert_eq!(search_results_at(&base, query, 0).await.unwrap().len(), 1);
        assert!(matches!(
            search_results_at(&base, query, 1).await,
            Err(EngineError::Blocked { .. })
        ));
        assert_eq!(search_results_at(&base, query, 1).await.unwrap().len(), 1);
        assert_eq!(search_results_at(&base, query, 2).await.unwrap().len(), 1);
        assert!(search_results_at(&base, query, 3).await.unwrap().is_empty());
        let requests = server.await.unwrap();
        assert_eq!(requests[1], requests[2]);
        for (body, offset, token) in [
            (&requests[1], "30", "first"),
            (&requests[3], "50", "second"),
        ] {
            let url = Url::parse(&format!("http://localhost/?{body}")).unwrap();
            let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(fields["s"], offset);
            assert_eq!(fields["vqd"], token);
            assert_eq!(fields["api"], "d.js");
            assert_eq!(fields["nextParams"], "a&b");
        }
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
