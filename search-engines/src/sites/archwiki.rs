//! Arch Wiki (<https://wiki.archlinux.org>) — the best Linux/systems
//! documentation on the internet, and the reason a programmer's search
//! engine should special-case it: a query like "systemd timers" or "network
//! configuration" almost always wants the Arch Wiki page, not a blog post
//! quoting it.
//!
//! Served by the same MediaWiki Action API as Wikipedia, with three
//! differences that matter:
//!
//! * Articles live under `/title/<Title>`, **not** `/wiki/<Title>` — the
//!   `/wiki/` prefix returns 403 here, so a Wikipedia-shaped URL builder
//!   produces links that look right and are all broken.
//! * The API is at `/api.php`; the more common `/w/api.php` layout is 403.
//! * Paging is a raw result offset (`sroffset`), not a page number, so
//!   `start` passes straight through with no arithmetic.
//!
//! This wiki has no CirrusSearch extension installed, so `list=search` falls
//! back to MediaWiki's built-in database search. Two consequences: the
//! default `srwhat` matches *titles* only (a multi-word natural-language
//! query with no matching title legitimately returns zero hits), and
//! `snippet` is the head of the page's raw wikitext rather than a
//! relevance-selected extract — which is why some snippets read like
//! `#REDIRECT [[Foo]]` and a few are empty.

use async_trait::async_trait;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use scraper::Html;
use serde::Deserialize;

use super::{encode, get_html, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct ArchWiki;

impl EngineInfo for ArchWiki {
    fn name(&self) -> &'static str {
        "Arch Wiki"
    }
}

/// `srlimit` we ask for. The API caps anonymous callers at 50; 20 keeps a
/// page cheap and matches what the cache layer pages by.
const RESULTS_PER_PAGE: usize = 20;

/// Characters that would change the *meaning* of an article path rather than
/// just its spelling. Deliberately narrower than `NON_ALPHANUMERIC`: `/`
/// separates Arch Wiki subpages (`Systemd/Timers`) and parentheses appear in
/// every translated title (`Network configuration (Español)`), so encoding
/// those would either break the link or make it unreadable. Non-ASCII bytes
/// are percent-encoded unconditionally by `utf8_percent_encode`.
const TITLE_ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

fn build_search_url(query: &str, start: usize) -> String {
    format!(
        "https://wiki.archlinux.org/api.php?action=query&list=search\
         &srsearch={}&format=json&formatversion=2&srlimit={RESULTS_PER_PAGE}\
         &sroffset={start}&srprop=snippet%7Cwordcount",
        encode(query)
    )
}

/// Turns an API `title` into the page a human can open. MediaWiki treats
/// spaces and underscores as the same character in a title, and the
/// canonical URL form is the underscore one.
fn build_article_url(title: &str) -> String {
    let path = title.replace(' ', "_");
    format!(
        "https://wiki.archlinux.org/title/{}",
        utf8_percent_encode(&path, TITLE_ESCAPE)
    )
}

/// `snippet` is HTML: the matched terms are wrapped in
/// `<span class="searchmatch">` and the surrounding text is entity-escaped.
/// Parsing it as a fragment strips the markup and decodes both named and
/// numeric character references in one pass.
fn strip_snippet_html(snippet: &str) -> String {
    let fragment = Html::parse_fragment(snippet);
    let text: String = fragment.root_element().text().collect();
    tidy(&text)
}

#[derive(Deserialize)]
struct SearchResponse {
    query: Option<QueryBlock>,
    /// Present instead of `query` when the API rejects the request (bad
    /// parameter, disabled module). Without this the failure would surface
    /// as "zero results" and look like genuine exhaustion.
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    info: String,
}

#[derive(Deserialize)]
struct QueryBlock {
    #[serde(default)]
    search: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct SearchHit {
    title: String,
    #[serde(default)]
    snippet: String,
}

// Note on `continue.sroffset`: it is *not* consulted here. A final partial
// page arrives with results and no `continue` block, so gating on it would
// silently discard the tail of every result set (the common case on this
// wiki, where a title search often has fewer than 20 hits). Exhaustion is
// instead signalled the way the trait expects — one offset past the end
// returns `"search": []`, which parses to an empty vec.
fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let response: SearchResponse = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "Arch Wiki returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    if let Some(ApiError { code, info }) = response.error {
        return Err(EngineError::ParseError(format!(
            "Arch Wiki API rejected the request: {code} ({info})"
        )));
    }

    let Some(query) = response.query else {
        return Ok(Vec::new());
    };

    Ok(query
        .search
        .into_iter()
        .filter(|hit| !hit.title.trim().is_empty())
        .map(|hit| {
            let description = truncate(&strip_snippet_html(&hit.snippet), 300);
            RawResult {
                url: build_article_url(&hit.title),
                // Every result must carry a description, and a handful of
                // pages (stubs, translation shells) genuinely have an empty
                // snippet — name the page rather than emit a blank row.
                description: if description.is_empty() {
                    format!("Arch Wiki article: {}", hit.title)
                } else {
                    description
                },
                title: hit.title,
            }
        })
        .collect())
}

#[async_trait]
impl SearchEngine for ArchWiki {
    /// `count` is ignored: the wiki pages by a fixed `srlimit` and the cache
    /// layer only needs "one more page from this engine".
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        // `get_html` rather than `get_json`: it is just "GET, and turn a
        // 429/403/5xx into EngineError::Blocked", which is what we want —
        // deserialization happens in the pure parser below so tests can
        // drive it from a fixture.
        let body = get_html(&build_search_url(query, start), "Arch Wiki").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_passes_start_through_as_a_raw_result_offset() {
        assert_eq!(
            build_search_url("pacman", 0),
            "https://wiki.archlinux.org/api.php?action=query&list=search\
             &srsearch=pacman&format=json&formatversion=2&srlimit=20\
             &sroffset=0&srprop=snippet%7Cwordcount"
        );
        // 40 is an offset, not a page index — the API takes it verbatim.
        assert_eq!(
            build_search_url("pacman", 40),
            "https://wiki.archlinux.org/api.php?action=query&list=search\
             &srsearch=pacman&format=json&formatversion=2&srlimit=20\
             &sroffset=40&srprop=snippet%7Cwordcount"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii_queries() {
        assert!(
            build_search_url("network configuration", 0)
                .contains("&srsearch=network%20configuration&")
        );
        assert!(
            build_search_url("café 日本語", 0)
                .contains("&srsearch=caf%C3%A9%20%E6%97%A5%E6%9C%AC%E8%AA%9E&")
        );
    }

    #[test]
    fn build_article_url_uses_the_title_prefix_not_wikipedias_wiki_prefix() {
        // `/wiki/<Title>` is a 403 on this host; only `/title/` resolves.
        assert_eq!(
            build_article_url("Network configuration"),
            "https://wiki.archlinux.org/title/Network_configuration"
        );
    }

    #[test]
    fn build_article_url_percent_encodes_reserved_characters_but_keeps_subpaths() {
        assert_eq!(
            build_article_url("Systemd/Timers"),
            "https://wiki.archlinux.org/title/Systemd/Timers"
        );
        assert_eq!(
            build_article_url("Network configuration (Español)"),
            "https://wiki.archlinux.org/title/Network_configuration_(Espa%C3%B1ol)"
        );
        // A `?`/`&`/`#` left raw would truncate the path into a query
        // string or fragment and point at the wrong page entirely.
        assert_eq!(
            build_article_url("AT&T C++ #1 what?"),
            "https://wiki.archlinux.org/title/AT%26T_C%2B%2B_%231_what%3F"
        );
    }

    #[test]
    fn strip_snippet_html_removes_match_markup_and_decodes_entities() {
        assert_eq!(
            strip_snippet_html(r#"Use <span class="searchmatch">pacman</span> -Syu to update"#),
            "Use pacman -Syu to update"
        );
        assert_eq!(
            strip_snippet_html("Tom &amp; Jerry &lt;tag&gt; &quot;quoted&quot; &#39;q&#39; &#x41;"),
            "Tom & Jerry <tag> \"quoted\" 'q' A"
        );
        // Snippets arrive with embedded newlines from the wikitext.
        assert_eq!(
            strip_snippet_html("[[Category:Network]]\n[[cs:Network]]\n"),
            "[[Category:Network]] [[cs:Network]]"
        );
    }

    #[test]
    fn parse_response_reads_every_result_from_the_recorded_fixture() {
        let results = parse_response(&fixture("archwiki.json")).unwrap();

        assert_eq!(results.len(), 20);
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "the contract requires all three fields on every result; the \
             fixture deliberately includes a page whose snippet is empty"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://wiki.archlinux.org/title/")),
            "results must link to article pages, never to api.php"
        );
    }

    #[test]
    fn parse_response_first_fixture_result_matches_the_recorded_values() {
        let results = parse_response(&fixture("archwiki.json")).unwrap();

        assert_eq!(results[0].title, "Network Configuration");
        assert_eq!(
            results[0].url,
            "https://wiki.archlinux.org/title/Network_Configuration"
        );
        assert_eq!(
            results[0].description,
            "#REDIRECT [[Network configuration]]"
        );
    }

    #[test]
    fn parse_response_names_the_page_when_the_fixture_snippet_is_empty() {
        let results = parse_response(&fixture("archwiki.json")).unwrap();

        let stub = results
            .iter()
            .find(|r| r.title == "Network Configuration (日本語)")
            .expect("fixture should contain the empty-snippet page");
        assert_eq!(
            stub.description,
            "Arch Wiki article: Network Configuration (日本語)"
        );
        assert_eq!(
            stub.url,
            "https://wiki.archlinux.org/title/Network_Configuration_(%E6%97%A5%E6%9C%AC%E8%AA%9E)"
        );
    }

    #[test]
    fn parse_response_treats_a_well_formed_empty_result_set_as_exhaustion() {
        // What the API returns one offset past the last hit: no `continue`
        // block, an empty `search` array, and no error.
        let empty = r#"{"batchcomplete":true,"query":{"searchinfo":{"totalhits":4},"search":[]}}"#;
        assert!(parse_response(empty).unwrap().is_empty());
    }

    #[test]
    fn parse_response_reports_an_api_error_instead_of_pretending_to_be_exhausted() {
        let error = r#"{"error":{"code":"unknown_action","info":"Unrecognized value."}}"#;
        assert!(matches!(
            parse_response(error),
            Err(EngineError::ParseError(_))
        ));
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_json() {
        assert!(parse_response("<html>blocked</html>").is_err());
    }

    #[ignore = "hits the live Arch Wiki API"]
    #[tokio::test]
    async fn test_archwiki_search_live() {
        let results = ArchWiki
            .search_results("network configuration", 0, 20)
            .await
            .unwrap();
        assert!(!results.is_empty());
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://wiki.archlinux.org/title/"))
        );
    }

    /// Guards the offset contract: page two must not repeat page one, and a
    /// far-past-the-end offset must come back empty rather than wrapping.
    #[ignore = "hits the live Arch Wiki API"]
    #[tokio::test]
    async fn test_archwiki_pagination_live() {
        let page1 = ArchWiki.search_results("network", 0, 20).await.unwrap();
        let page2 = ArchWiki.search_results("network", 20, 20).await.unwrap();

        assert!(!page1.is_empty());
        assert!(
            page2.iter().all(|b| !page1.iter().any(|a| a.url == b.url)),
            "sroffset should advance, not repeat page 1"
        );

        let past_end = ArchWiki
            .search_results("network", 100_000, 20)
            .await
            .unwrap();
        assert!(past_end.is_empty());
    }
}
