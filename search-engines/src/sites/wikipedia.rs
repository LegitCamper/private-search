//! English Wikipedia article search via the MediaWiki Action API
//! (`en.wikipedia.org/w/api.php`, `action=query&list=search`).
//!
//! This is the encyclopedia lane of the engine set: for "what *is* an Erdős
//! number", the answer is the article itself, not a web page discussing it.
//! No auth, no key, no quota registration.
//!
//! Non-obvious things about this API:
//!
//! * **Paging is a raw result offset, not a page number.** `sroffset` is
//!   "skip this many hits", so `start` passes straight through with no
//!   arithmetic — unlike every page-numbered sibling in this directory.
//! * **Exhaustion shows up two ways, and only one is load-bearing.** Past
//!   the last hit the API returns `search: []` *and* drops the top-level
//!   `continue` object. The empty array is what this engine returns as the
//!   empty vec the cache layer reads as exhaustion; `continue` is decoded
//!   only for documentation value, since a stateless per-page call has
//!   nothing to carry it forward to.
//! * **`snippet` is HTML, not text.** MediaWiki wraps every matched term in
//!   `<span class="searchmatch">` and escapes the rest (`&quot;`, `&amp;`,
//!   `&#39;`). Both the tags and the entities have to come off before the
//!   snippet is fit for a result row. There is no plain-text option.
//! * **`list=search` returns titles, never URLs.** The article URL is
//!   reconstructed here, and MediaWiki's own escaping rule is idiosyncratic:
//!   spaces become underscores and it leaves `( ) , ! : ; @ $ * / ~` literal
//!   while percent-encoding `'`, `&`, `+` and all non-ASCII. Matching it
//!   exactly matters — a link that merely *redirects* to the canonical form
//!   is a wasted round trip for every user who clicks it.
//! * **`formatversion=2`** is what makes the response shape sane: `search`
//!   is a plain array of objects and booleans are real JSON booleans. The
//!   legacy version 1 wraps text in `*` keys and spells `true` as `""`.
//! * **Wikimedia rate-limits by IP *and* User-Agent**, and the response
//!   carries `Vary: User-Agent`. Their API etiquette asks clients to
//!   identify themselves, so this engine overrides the shared Firefox UA —
//!   see [`USER_AGENT`].

use async_trait::async_trait;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use super::{encode, get_json_with, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Wikipedia;

impl EngineInfo for Wikipedia {
    fn name(&self) -> &'static str {
        "Wikipedia"
    }
}

const ENGINE: &str = "Wikipedia";

/// `srlimit`. 50 is the anonymous maximum, but the merge layer consumes a
/// page at a time and Wikimedia's rate limiter counts requests, not hits —
/// so there is no reason to ask for more than one screenful.
const PER_PAGE: usize = 20;

/// Wikimedia's API etiquette asks for a User-Agent that names the client and
/// gives somewhere to complain to; generic browser strings are the ones they
/// throttle first, and responses carry `Vary: User-Agent`.
///
/// The tradeoff: this drops the shared client's Firefox camouflage for
/// Wikipedia only. That is the right call *here* — the endpoint is a public
/// API with no bot wall, so honesty costs nothing and buys goodwill — but it
/// would be exactly the wrong call for the scraping engines, whose HTML
/// endpoints refuse anything that doesn't look like a browser.
const USER_AGENT: &str = "private-search/0.1 (https://github.com/private-search/private-search; \
     self-hosted metasearch aggregator)";

/// MediaWiki's `wfUrlencode`: percent-encode, then hand back the sub-delims
/// it considers safe in a path. Reproduced rather than approximated so the
/// links we emit are byte-identical to Wikipedia's own canonical URLs.
const TITLE_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'!')
    .remove(b'*')
    .remove(b'(')
    .remove(b')')
    .remove(b';')
    .remove(b':')
    .remove(b'@')
    .remove(b'$')
    .remove(b',')
    .remove(b'/');

/// `srsearch` is read out of PHP's `$_GET`, which decodes `+` as a space.
/// The shared [`encode`] leaves `+` literal (GitHub-style qualifier syntax
/// needs it), so searching "c++" would silently become "c  " without this.
fn encode_query(query: &str) -> String {
    encode(query).replace('+', "%2B")
}

/// `start` is an absolute hit offset here, so it becomes `sroffset` as-is —
/// no page-number conversion, and no dropped remainder when the caller's
/// `start` isn't a multiple of [`PER_PAGE`].
fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://en.wikipedia.org/w/api.php\
         ?action=query&list=search&srsearch={}\
         &format=json&formatversion=2&srlimit={PER_PAGE}&sroffset={start}\
         &srprop=snippet%7Cwordcount",
        encode_query(query)
    )
}

/// Turns a search hit's title into the article permalink a human can open.
fn article_url(title: &str) -> String {
    let underscored = title.replace(' ', "_");
    let path = utf8_percent_encode(&underscored, TITLE_ESCAPE);
    format!("https://en.wikipedia.org/wiki/{path}")
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    query: Option<Query>,
    /// Present only while more hits remain. Not acted on — a stateless
    /// per-page call has nowhere to carry it — because the same condition
    /// already shows up as an empty `search` array on the next request.
    #[serde(rename = "continue", default)]
    #[allow(dead_code)]
    continue_: Option<Continue>,
    /// MediaWiki reports bad parameters as a JSON `error` object on an
    /// HTTP 200, so a status check alone would let it through as "0 results".
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Continue {
    #[serde(default)]
    #[allow(dead_code)]
    sroffset: usize,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    info: String,
}

#[derive(Deserialize)]
struct Query {
    #[serde(default)]
    search: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    #[serde(default)]
    title: String,
    /// HTML, not text — see the module docs.
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    wordcount: u64,
}

/// Drops HTML tags. Safe to do character-wise because MediaWiki escapes any
/// literal `<` in the article text as `&lt;`, so an unescaped `<` in a
/// snippet is always the start of a real tag.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

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

/// Decodes the HTML entities MediaWiki escapes snippet text with. Kept small
/// and local: the API escapes only what it must, and article prose that
/// needed the full WHATWG named-entity table would be an outlier.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];

        // Real entities are short. A `;` further out belongs to ordinary
        // prose punctuation, e.g. "Tom & Jerry; also known as...".
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

/// Tags come off before entities are decoded, so a snippet containing an
/// escaped `&lt;script&gt;` renders as literal text instead of being
/// re-read as markup by the stripper.
fn snippet_to_text(snippet: &str) -> String {
    decode_entities(&strip_tags(snippet))
}

/// The contract requires a non-empty description. A hit whose snippet is
/// empty (a stub, or a match that landed only in the title) still has a word
/// count, which at least tells the reader how substantial the article is.
fn describe(hit: &Hit) -> String {
    let snippet = truncate(&tidy(&snippet_to_text(&hit.snippet)), 300);
    if !snippet.is_empty() {
        return snippet;
    }

    match hit.wordcount {
        0 => "English Wikipedia article".to_string(),
        n => format!("English Wikipedia article · {n} words"),
    }
}

pub fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: Response = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "{ENGINE} returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    to_results(response)
}

/// The half of parsing that runs on an already-deserialized body, so the live
/// path can let `get_json_with` do the deserializing and the tests can drive
/// the same logic from a fixture string.
fn to_results(response: Response) -> Result<Vec<RawResult>, EngineError> {
    if let Some(error) = &response.error {
        return Err(EngineError::ParseError(format!(
            "{ENGINE} API error {}: {}",
            error.code, error.info
        )));
    }

    let Some(query) = response.query else {
        return Ok(Vec::new());
    };

    Ok(query
        .search
        .iter()
        // An empty title would build a bare `/wiki/` link to nowhere.
        .filter(|hit| !hit.title.is_empty())
        .map(|hit| RawResult {
            url: article_url(&hit.title),
            title: hit.title.clone(),
            description: describe(hit),
        })
        .collect())
}

#[async_trait]
impl SearchEngine for Wikipedia {
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // Per-request headers win over the shared client's defaults, so this
        // replaces the Firefox User-Agent rather than sending a second one.
        let response: Response = get_json_with(
            &build_search_url(query, start),
            ENGINE,
            &[("User-Agent", USER_AGENT), ("Accept", "application/json")],
        )
        .await?;

        to_results(response)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_passes_a_zero_start_through_as_a_zero_offset() {
        assert_eq!(
            build_search_url("rust async", 0),
            "https://en.wikipedia.org/w/api.php\
             ?action=query&list=search&srsearch=rust%20async\
             &format=json&formatversion=2&srlimit=20&sroffset=0\
             &srprop=snippet%7Cwordcount"
        );
    }

    /// The offset is absolute, so an arbitrary `start` is not rounded down to
    /// a page boundary the way the page-numbered engines round it.
    #[test]
    fn build_search_url_passes_a_later_start_through_as_a_raw_offset() {
        assert_eq!(
            build_search_url("rust async", 40),
            "https://en.wikipedia.org/w/api.php\
             ?action=query&list=search&srsearch=rust%20async\
             &format=json&formatversion=2&srlimit=20&sroffset=40\
             &srprop=snippet%7Cwordcount"
        );
        assert!(build_search_url("rust async", 37).ends_with("&srprop=snippet%7Cwordcount"));
        assert!(build_search_url("rust async", 37).contains("&sroffset=37&"));
    }

    /// A raw space or `é` in `srsearch` would corrupt the query string; a raw
    /// `&` would inject a parameter.
    #[test]
    fn build_search_url_escapes_spaces_non_ascii_and_ampersands() {
        assert_eq!(
            build_search_url("café & 日本語", 0),
            "https://en.wikipedia.org/w/api.php\
             ?action=query&list=search\
             &srsearch=caf%C3%A9%20%26%20%E6%97%A5%E6%9C%AC%E8%AA%9E\
             &format=json&formatversion=2&srlimit=20&sroffset=0\
             &srprop=snippet%7Cwordcount"
        );
    }

    /// PHP decodes a literal `+` in a query string as a space, which would
    /// turn a search for "c++" into a search for "c".
    #[test]
    fn build_search_url_escapes_a_plus_so_php_does_not_read_it_as_a_space() {
        assert!(build_search_url("c++", 0).contains("&srsearch=c%2B%2B&"));
    }

    #[test]
    fn article_url_underscores_spaces_and_leaves_parentheses_literal() {
        assert_eq!(
            article_url("Rust (programming language)"),
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    /// Pinned against the `canonicalurl` Wikipedia itself reports for these
    /// titles, so we link straight to the article instead of to a redirect.
    #[test]
    fn article_url_percent_encodes_non_ascii_and_reserved_characters() {
        assert_eq!(
            article_url("Erdős number"),
            "https://en.wikipedia.org/wiki/Erd%C5%91s_number"
        );
        assert_eq!(article_url("AT&T"), "https://en.wikipedia.org/wiki/AT%26T");
        assert_eq!(article_url("C++"), "https://en.wikipedia.org/wiki/C%2B%2B");
        assert_eq!(
            article_url("Who's Who"),
            "https://en.wikipedia.org/wiki/Who%27s_Who"
        );
        assert_eq!(
            article_url("Hello, world!"),
            "https://en.wikipedia.org/wiki/Hello,_world!"
        );
    }

    #[test]
    fn snippet_to_text_strips_searchmatch_spans_and_decodes_entities() {
        // A decimal entity (`&#8212;`) and a hex one (`&#x27;`) take
        // different branches of the numeric decoder.
        assert_eq!(
            snippet_to_text(
                r#"<span class="searchmatch">Rust</span> is &quot;fast&quot; &amp; safe &#8212; per Mozilla&#x27;s docs"#
            ),
            "Rust is \"fast\" & safe — per Mozilla's docs"
        );
    }

    /// Stripping before decoding is what keeps an escaped tag as text; the
    /// other order would feed `<script>` back to the tag stripper.
    #[test]
    fn snippet_to_text_keeps_an_escaped_tag_as_literal_text() {
        assert_eq!(
            snippet_to_text("use &lt;script&gt; sparingly"),
            "use <script> sparingly"
        );
    }

    #[test]
    fn decode_entities_leaves_a_bare_ampersand_alone() {
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("A & B; C"), "A & B; C");
    }

    #[test]
    fn parse_response_reads_a_full_page_from_the_real_fixture() {
        let results = parse_response(&fixture("wikipedia.json")).unwrap();
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn parse_response_leaves_no_field_empty_on_the_real_fixture() {
        let results = parse_response(&fixture("wikipedia.json")).unwrap();
        assert!(
            results.iter().all(|r| r.url.starts_with("https://")
                && !r.title.is_empty()
                && !r.description.is_empty()),
            "every result must be openable and readable"
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("wikipedia.json")).unwrap();
        assert_eq!(results[0].title, "Erdős–Rényi model");
        assert_eq!(
            results[0].url,
            "https://en.wikipedia.org/wiki/Erd%C5%91s%E2%80%93R%C3%A9nyi_model"
        );
    }

    #[test]
    fn parse_response_leaves_no_markup_or_entities_in_descriptions_on_the_real_fixture() {
        let results = parse_response(&fixture("wikipedia.json")).unwrap();
        assert!(
            results
                .iter()
                .all(|r| !r.description.contains('<') && !r.description.contains("&quot;")),
            "searchmatch spans and entities must not reach the result row"
        );
    }

    /// Past the last hit the API answers with a well-formed body carrying an
    /// empty `search` array and no `continue` — exhaustion, not an error.
    #[test]
    fn parse_response_reads_an_exhausted_result_set_as_an_empty_vec() {
        let json = r#"{"batchcomplete":true,"query":{"searchinfo":{"totalhits":237},"search":[]}}"#;
        assert_eq!(parse_response(json).unwrap().len(), 0);
    }

    #[test]
    fn parse_response_reads_a_zero_hit_search_as_an_empty_vec() {
        let json = r#"{"batchcomplete":true,"query":{"searchinfo":{"totalhits":0},"search":[]}}"#;
        assert!(parse_response(json).unwrap().is_empty());
    }

    #[test]
    fn parse_response_surfaces_a_json_api_error_returned_on_an_http_200() {
        let json =
            r#"{"error":{"code":"nosrsearch","info":"The \"srsearch\" parameter must be set."}}"#;
        let err = parse_response(json).unwrap_err();
        assert!(matches!(err, EngineError::ParseError(ref m) if m.contains("nosrsearch")));
    }

    #[ignore]
    #[tokio::test]
    async fn test_wikipedia_search_live() {
        let results = Wikipedia
            .search_results("erdos number", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://en.wikipedia.org/wiki/")
                    && !r.title.is_empty()
                    && !r.description.is_empty())
        );
    }

    #[ignore]
    #[tokio::test]
    async fn test_wikipedia_offset_paging_does_not_repeat_live() {
        let page1 = Wikipedia
            .search_results("erdos number", 0, 20)
            .await
            .unwrap();
        let page2 = Wikipedia
            .search_results("erdos number", PER_PAGE, 20)
            .await
            .unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "sroffset should advance, not repeat the first page"
        );
    }
}
