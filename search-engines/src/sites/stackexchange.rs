//! Stack Exchange (Stack Overflow) question search via the public
//! `api.stackexchange.com` v2.3 API.
//!
//! For a programmer-facing search this is one of the highest-signal engines
//! in the set: a query like "borrow checker closure" should surface the
//! actual Q&A thread, not a blog post quoting it.
//!
//! Non-obvious things about this API:
//!
//! * **Quota is per-IP, not per-query: ~300 requests/day unauthenticated.**
//!   That is by far the tightest budget of any engine here; every response
//!   echoes `quota_max`/`quota_remaining` so the ceiling is observable.
//!   Registering an app key raises it to 10,000/day.
//! * **Page 25 is a hard wall.** `page` above 25 returns HTTP 400 with
//!   `"page above 25 requires access token or app key"`, so unauthenticated
//!   depth tops out at 500 results. We stop before asking.
//! * **Errors and throttling arrive as JSON, not just as status codes.** A
//!   healthy 200 may still carry a top-level `backoff: N`, meaning "make no
//!   further requests to this method for N seconds". Detecting it requires
//!   reading the body ourselves, so this engine calls `browser_client()` /
//!   `body_or_block()` directly rather than going through `get_json`.
//! * **Bodies are always gzip-encoded**, even when the request does not ask
//!   for it. `browser_client()` sets `.gzip(true)`, so reqwest decompresses
//!   transparently and we never see the raw stream.
//! * **Titles are HTML-escaped** (`&quot;`, `&#39;`, `&amp;`). They are
//!   decoded here — the API has no "give me plain text" option.
//! * The default filter omits the question body entirely, so the
//!   description is synthesized from score/answers/tags instead.

use async_trait::async_trait;
use serde::Deserialize;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use super::{encode, page_number, tidy, truncate};
use crate::{
    BlockKind, EngineError, EngineInfo, RawResult, SearchEngine, body_or_block, browser_client,
};

#[derive(Clone)]
pub struct StackExchange;

impl EngineInfo for StackExchange {
    fn name(&self) -> &'static str {
        "StackExchange"
    }
}

const ENGINE: &str = "StackExchange";

/// The API's `pagesize`. 100 is the documented maximum, but a page of 20
/// keeps each request cheap against the 300/day quota and matches what the
/// merge layer consumes at a time.
const PER_PAGE: usize = 20;

/// Highest `page` the API serves without an app key; beyond it every request
/// is a wasted quota unit that returns HTTP 400.
const MAX_PAGE: usize = 25;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://api.stackexchange.com/2.3/search/advanced\
         ?order=desc&sort=relevance&q={}&site=stackoverflow&pagesize={PER_PAGE}&page={page}",
        encode(query)
    )
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    items: Vec<Item>,
    /// Seconds the API wants us to wait before calling this method again.
    /// Present on otherwise-successful responses.
    #[serde(default)]
    backoff: Option<u64>,
    /// `has_more` can't be acted on within a single stateless call — the
    /// caller asks for one page at a time — but when it is false the next
    /// page comes back with `items: []`, which parses to the empty vec the
    /// cache layer reads as exhaustion. Kept for documentation value.
    #[serde(default)]
    #[allow(dead_code)]
    has_more: bool,
    #[serde(default)]
    #[allow(dead_code)]
    quota_remaining: Option<u32>,
    #[serde(default)]
    error_name: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    /// Human-facing `https://stackoverflow.com/questions/...` permalink —
    /// already absolute, and distinct from the API's own resource URL.
    #[serde(default)]
    link: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    answer_count: u32,
    #[serde(default)]
    is_answered: bool,
    #[serde(default)]
    tags: Vec<String>,
}

/// Decodes the HTML entities Stack Exchange escapes titles with. Deliberately
/// small: the API escapes only what it must, and pulling in a full HTML
/// entity crate for five names plus numeric references isn't worth it.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];

        // Real entities are short. Anything further out is a stray `&`
        // followed later by ordinary punctuation, e.g. "A & B; C".
        let terminator = after
            .char_indices()
            .take_while(|(i, _)| *i < 10)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i);

        match terminator.and_then(|end| decode_entity(&after[..end]).map(|ch| (ch, end))) {
            Some((ch, end)) => {
                out.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }

    out.push_str(rest);
    out
}

/// Resolves the inside of an entity (`quot`, `#39`, `#x27`) to a character.
fn decode_entity(body: &str) -> Option<char> {
    match body {
        "quot" => Some('"'),
        "apos" => Some('\''),
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "nbsp" => Some(' '),
        _ => {
            let digits = body.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Builds the one-line description. The default filter returns no question
/// body, so the useful signal is the metadata: how the community voted, and
/// whether anyone actually answered.
fn describe(item: &Item) -> String {
    let answers = match item.answer_count {
        0 => "no answers".to_string(),
        1 => "1 answer".to_string(),
        n => format!("{n} answers"),
    };

    let mut description = format!("Score {} · {answers}", item.score);
    if item.is_answered {
        description.push_str(" · answered");
    }
    if !item.tags.is_empty() {
        description.push_str(" — ");
        description.push_str(&item.tags.join(", "));
    }

    description
}

/// One parsed page: the hits, plus any `backoff` the API attached to an
/// otherwise-successful response.
pub struct Page {
    pub results: Vec<RawResult>,
    pub backoff: Option<Duration>,
}

/// Convenience wrapper for callers (and tests) that only want the hits.
pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    parse_page(json).map(|page| page.results)
}

pub fn parse_page(json: &str) -> Result<Page, EngineError> {
    let response: Response = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "{ENGINE} returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    // A `backoff` means "make no *further* requests to this method for N
    // seconds" — it does not invalidate the hits in this very response. It is
    // returned alongside them so the caller can gate the *next* request;
    // discarding a perfectly good page here used to throw away results the
    // user had already waited for.
    let backoff = response.backoff.map(Duration::from_secs);

    // Quota exhaustion and throttling come back as a JSON error object, on
    // an HTTP 400 that `body_or_block` doesn't classify on its own.
    if let Some(name) = &response.error_name {
        let detail = response
            .error_message
            .clone()
            .unwrap_or_else(|| name.clone());
        let kind = match name.as_str() {
            "throttle_violation" => BlockKind::RateLimited,
            _ => BlockKind::AccessDenied,
        };
        return Err(EngineError::Blocked {
            kind,
            // A throttle error usually arrives with the backoff that says
            // how long to stay away; honour it instead of guessing.
            retry_after: backoff,
            detail: format!("{ENGINE} API error: {detail}"),
        });
    }

    let results = response
        .items
        .iter()
        .filter(|item| item.link.starts_with("https://") && !item.title.is_empty())
        .map(|item| {
            let description = describe(item);
            RawResult {
                url: item.link.clone(),
                title: decode_entities(&item.title),
                description: truncate(&tidy(&description), 300),
            }
        })
        .collect();

    Ok(Page { results, backoff })
}

/// When the API's last `backoff` expires. A backoff applies to the *next*
/// request, so it is remembered here rather than turned into an error that
/// throws away the page it arrived with.
static BACKOFF_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

fn backoff_remaining() -> Option<Duration> {
    let mut guard = BACKOFF_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    let until = (*guard)?;
    match until.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => Some(remaining),
        _ => {
            *guard = None;
            None
        }
    }
}

fn remember_backoff(backoff: Option<Duration>) {
    let Some(backoff) = backoff else { return };
    let mut guard = BACKOFF_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    // Keep the longest pause: a concurrent response carrying a shorter
    // backoff must not shorten one the API already asked for.
    *guard = (*guard).max(Instant::now().checked_add(backoff));
}

#[cfg(test)]
fn forget_backoff() {
    *BACKOFF_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[async_trait]
impl SearchEngine for StackExchange {
    /// Fixed upstream page size: the cursor must land on a page
    /// boundary, never on "however many rows survived filtering".
    fn page_size(&self) -> Option<usize> {
        Some(PER_PAGE)
    }

    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if page_number(start, PER_PAGE) > MAX_PAGE {
            return Ok(Vec::new());
        }

        // The API asked us to stay off this method for a while. Spending a
        // quota unit to be told that again is exactly the futile traffic the
        // cooldown layer exists to avoid.
        if let Some(remaining) = backoff_remaining() {
            return Err(EngineError::Blocked {
                kind: BlockKind::RateLimited,
                retry_after: Some(remaining),
                detail: format!("{ENGINE} asked for a {}s backoff", remaining.as_secs()),
            });
        }

        // Not `get_json`: a `backoff` field has to be read off the parsed
        // body, and the API signals throttling in the payload rather than
        // only in the status line.
        let resp = browser_client()
            .get(build_search_url(query, start))
            .send()
            .await
            .map_err(EngineError::ReqwestError)?;

        let body = body_or_block(resp, ENGINE).await?;
        let page = parse_page(&body)?;
        remember_backoff(page.backoff);
        Ok(page.results)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_the_first_page_for_a_zero_start() {
        assert_eq!(
            build_search_url("rust async", 0),
            "https://api.stackexchange.com/2.3/search/advanced\
             ?order=desc&sort=relevance&q=rust%20async&site=stackoverflow&pagesize=20&page=1"
        );
    }

    #[test]
    fn build_search_url_advances_the_page_for_a_later_start() {
        assert_eq!(
            build_search_url("rust async", PER_PAGE * 2),
            "https://api.stackexchange.com/2.3/search/advanced\
             ?order=desc&sort=relevance&q=rust%20async&site=stackoverflow&pagesize=20&page=3"
        );
    }

    /// A raw space or a raw `é` in `q` would corrupt the query string; a raw
    /// `&` would inject a parameter.
    #[test]
    fn build_search_url_escapes_spaces_non_ascii_and_ampersands() {
        assert_eq!(
            build_search_url("café & 日本語", 0),
            "https://api.stackexchange.com/2.3/search/advanced\
             ?order=desc&sort=relevance&q=caf%C3%A9%20%26%20%E6%97%A5%E6%9C%AC%E8%AA%9E\
             &site=stackoverflow&pagesize=20&page=1"
        );
    }

    #[test]
    fn search_results_stops_before_the_unauthenticated_page_limit() {
        // Page 26 is HTTP 400 "page above 25 requires access token or app
        // key" — a wasted quota unit, so it is never requested.
        assert_eq!(page_number(PER_PAGE * MAX_PAGE, PER_PAGE), MAX_PAGE + 1);
    }

    #[test]
    fn parse_response_reads_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("stackexchange.json")).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("stackexchange.json")).unwrap();
        assert!(results.iter().all(|r| r.url.starts_with("https://")
            && !r.title.is_empty()
            && !r.description.is_empty()));
    }

    #[test]
    fn parse_response_maps_the_first_fixture_result_to_its_recorded_values() {
        let results = parse_response(&fixture("stackexchange.json")).unwrap();
        assert_eq!(
            results[0].url,
            "https://stackoverflow.com/questions/62619870/how-to-call-rust-async-method-from-python"
        );
        assert_eq!(
            results[0].title,
            "How to call Rust async method from Python?"
        );
        assert_eq!(
            results[0].description,
            "Score 5 · 3 answers · answered — python, rust, python-3.8, pyo3"
        );
    }

    /// The fixture carries genuinely escaped titles — this guards the
    /// decoder against the real input rather than only a synthetic one.
    #[test]
    fn parse_response_decodes_escaped_titles_in_the_real_fixture() {
        let results = parse_response(&fixture("stackexchange.json")).unwrap();
        assert!(results.iter().all(|r| !r.title.contains("&quot;")
            && !r.title.contains("&#39;")
            && !r.title.contains("&amp;")));
        assert!(
            results
                .iter()
                .any(|r| r.title.contains('"') || r.title.contains('\''))
        );
    }

    #[test]
    fn parse_response_decodes_named_and_numeric_html_entities_in_titles() {
        let json = r#"{
            "items": [{
                "link": "https://stackoverflow.com/questions/1/x",
                "title": "Why does &quot;a&amp;b&quot; break Bob&#39;s caf&#xe9; &lt;script&gt;?",
                "score": 1, "answer_count": 0, "is_answered": false, "tags": ["bash"]
            }],
            "has_more": false, "quota_max": 300, "quota_remaining": 297
        }"#;

        let results = parse_response(json).unwrap();
        assert_eq!(
            results[0].title,
            r#"Why does "a&b" break Bob's café <script>?"#
        );
    }

    /// A `&` that isn't an entity must survive verbatim, and must not eat the
    /// text after it.
    #[test]
    fn decode_entities_leaves_a_bare_ampersand_alone() {
        assert_eq!(decode_entities("AT&T; fine"), "AT&T; fine");
        assert_eq!(decode_entities("a & b"), "a & b");
        assert_eq!(decode_entities("&notanentity;"), "&notanentity;");
    }

    #[test]
    fn describe_synthesizes_a_description_when_the_question_has_no_metadata() {
        let bare = Item {
            link: "https://stackoverflow.com/questions/2/y".into(),
            title: "Bare".into(),
            score: 0,
            answer_count: 0,
            is_answered: false,
            tags: Vec::new(),
        };
        assert_eq!(describe(&bare), "Score 0 · no answers");
    }

    /// Exhaustion, not an error: the cache layer reads an empty vec as "this
    /// engine has no more pages".
    #[test]
    fn parse_response_treats_an_empty_result_set_as_exhaustion() {
        let json = r#"{"items":[],"has_more":false,"quota_max":300,"quota_remaining":296}"#;
        assert!(parse_response(json).unwrap().is_empty());
    }

    #[test]
    fn parse_page_reports_a_backoff_without_discarding_the_page() {
        let json = fixture("stackexchange.json")
            .replace(r#""has_more":true"#, r#""has_more":true,"backoff":10"#);
        let page = parse_page(&json).expect("a backoff must not fail the page");
        assert_eq!(page.backoff, Some(Duration::from_secs(10)));
        assert!(
            !page.results.is_empty(),
            "the hits that arrived with the backoff must still be served"
        );
    }

    #[test]
    fn a_remembered_backoff_blocks_the_next_request_then_expires() {
        forget_backoff();
        remember_backoff(Some(Duration::from_secs(30)));
        let long = backoff_remaining().expect("a fresh backoff must block");

        // A shorter concurrent backoff must not cut the longer pause short.
        remember_backoff(Some(Duration::from_secs(1)));
        assert!(backoff_remaining().unwrap() > Duration::from_secs(1));
        assert!(backoff_remaining().unwrap() <= long);

        forget_backoff();
        assert!(
            backoff_remaining().is_none(),
            "an elapsed backoff must not keep the engine blocked"
        );
    }

    #[test]
    fn a_throttle_error_carries_the_api_backoff_as_its_retry_after() {
        let json = r#"{"error_id":502,"error_message":"too many requests",
            "error_name":"throttle_violation","backoff":22}"#;
        match parse_response(json) {
            Err(EngineError::Blocked {
                kind, retry_after, ..
            }) => {
                assert_eq!(kind, BlockKind::RateLimited);
                assert_eq!(retry_after, Some(Duration::from_secs(22)));
            }
            other => panic!("expected a rate-limit block, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_surfaces_a_json_error_object_as_a_block() {
        let json = r#"{"error_id":403,
            "error_message":"page above 25 requires access token or app key",
            "error_name":"access_denied"}"#;
        match parse_response(json) {
            Err(EngineError::Blocked { kind, detail, .. }) => {
                assert_eq!(kind, BlockKind::AccessDenied);
                assert!(detail.contains("page above 25"));
            }
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_reports_a_non_json_body_as_a_parse_error() {
        assert!(matches!(
            parse_response("<html>gateway timeout</html>"),
            Err(EngineError::ParseError(_))
        ));
    }

    /// Confirms end to end that the API is reachable unauthenticated and
    /// that reqwest's `.gzip(true)` transparently decompresses the response
    /// (this API always gzips its body).
    #[ignore]
    #[tokio::test]
    async fn stackexchange_live_search_returns_results() {
        let results = StackExchange
            .search_results("rust async", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://stackoverflow.com/questions/"))
        );
    }
}
