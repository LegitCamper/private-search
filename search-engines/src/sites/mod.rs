//! Site-specific adapters: engines that search *one* site rather than the
//! whole web.
//!
//! A general web engine (Brave, DuckDuckGo) answers "what's on the internet
//! about X". These answer "what does site S have named X" — which is what a
//! programmer usually means. Searching "pangolin" should surface the
//! `pangolin` GitHub repo, not a page *about* the repo.
//!
//! Nearly all of these hit a public JSON API rather than scraping HTML.
//! That matters for more than tidiness: an API is a stable contract that
//! doesn't get redesigned out from under a CSS selector, and it isn't behind
//! the bot walls that make the HTML endpoints of these same sites unusable
//! from a server.

pub mod archwiki;
pub mod codeberg;
pub mod crates_io;
pub mod dockerhub;
pub mod github;
pub mod gitlab;
pub mod gopkg;
pub mod hackernews;
pub mod hexpm;
pub mod lobsters;
pub mod maven;
pub mod mdn;
pub mod nixpkgs;
pub mod npm;
pub mod nuget;
pub mod packagist;
pub mod pypi;
pub mod rubygems;
pub mod stackexchange;
pub mod wikipedia;

pub use archwiki::ArchWiki;
pub use codeberg::Codeberg;
pub use crates_io::CratesIo;
pub use dockerhub::DockerHub;
pub use github::GitHub;
pub use gitlab::GitLab;
pub use gopkg::GoPkg;
pub use hackernews::HackerNews;
pub use hexpm::HexPm;
pub use lobsters::Lobsters;
pub use maven::MavenCentral;
pub use mdn::Mdn;
pub use nixpkgs::NixPackages;
pub use npm::Npm;
pub use nuget::NuGet;
pub use packagist::Packagist;
pub use pypi::PyPi;
pub use rubygems::RubyGems;
pub use stackexchange::StackExchange;
pub use wikipedia::Wikipedia;

use crate::{EngineError, body_or_block, browser_client};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::de::DeserializeOwned;

/// Percent-encodes everything a query string can't carry literally. Narrower
/// than `NON_ALPHANUMERIC` (used by the web engines) because several of
/// these APIs use qualifier syntax — GitHub's `stars:>10`, StackExchange's
/// `tag:rust` — whose `:`/`>` must survive as-is to keep their meaning.
const QUERY_ESCAPE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'%')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'\\')
    .add(b'^');

pub(crate) fn encode(query: &str) -> String {
    utf8_percent_encode(query, QUERY_ESCAPE).to_string()
}

/// Which page `start` falls on, for APIs that page by 1-based page number.
pub(crate) fn page_number(start: usize, per_page: usize) -> usize {
    start / per_page.max(1) + 1
}

/// GETs `url` and deserializes the body as JSON.
///
/// Routes through [`body_or_block`] so an HTTP 429/403/5xx still becomes a
/// structured [`EngineError::Blocked`] (feeding the cooldown registry)
/// rather than a deserialization failure that looks like our bug.
pub(crate) async fn get_json<T: DeserializeOwned>(
    url: &str,
    engine: &'static str,
) -> Result<T, EngineError> {
    get_json_with(url, engine, &[]).await
}

/// [`get_json`] plus extra request headers — several of these APIs need a
/// specific `Accept` to select a response version (GitHub) or to be served
/// JSON at all (Docker Hub).
pub(crate) async fn get_json_with<T: DeserializeOwned>(
    url: &str,
    engine: &'static str,
    headers: &[(&'static str, &'static str)],
) -> Result<T, EngineError> {
    let mut request = browser_client().get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }

    let resp = request.send().await.map_err(EngineError::ReqwestError)?;
    let body = body_or_block(resp, engine).await?;

    serde_json::from_str(&body).map_err(|e| {
        EngineError::ParseError(format!(
            "{engine} returned a body that isn't the JSON we expect ({e}); \
             the API shape may have changed"
        ))
    })
}

/// GETs `url` and returns the body as text, for the handful of sites with no
/// usable JSON search API.
pub(crate) async fn get_html(url: &str, engine: &'static str) -> Result<String, EngineError> {
    let resp = browser_client()
        .get(url)
        .send()
        .await
        .map_err(EngineError::ReqwestError)?;
    body_or_block(resp, engine).await
}

/// Collapses runs of whitespace and trims — API descriptions routinely carry
/// newlines and double spaces that render badly in a one-line result row.
pub(crate) fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates `text` to at most `max` characters (not bytes — these
/// descriptions are frequently non-ASCII), appending an ellipsis if cut.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Reads a recorded API response from `tests/fixtures/sites/`. Unit tests
/// parse these instead of hitting the network, so a `cargo test` run can't
/// get the CI IP rate-limited by twenty different APIs at once.
#[cfg(test)]
pub(crate) fn fixture(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sites")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture tests/fixtures/sites/{relative}: {e}"))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn encode_escapes_spaces_but_keeps_qualifier_syntax() {
        assert_eq!(encode("rust async"), "rust%20async");
        assert_eq!(encode("tag:rust stars:>10"), "tag:rust%20stars:%3E10");
    }

    #[test]
    fn encode_escapes_non_ascii() {
        assert_eq!(encode("café"), "caf%C3%A9");
    }

    #[test]
    fn encode_escapes_ampersand_so_it_cannot_inject_a_query_parameter() {
        assert_eq!(encode("a&b=c"), "a%26b=c");
    }

    #[test]
    fn page_number_is_one_based() {
        assert_eq!(page_number(0, 20), 1);
        assert_eq!(page_number(19, 20), 1);
        assert_eq!(page_number(20, 20), 2);
        assert_eq!(page_number(41, 20), 3);
    }

    #[test]
    fn tidy_collapses_whitespace() {
        assert_eq!(tidy("  a\n\n  b\tc "), "a b c");
    }

    #[test]
    fn truncate_leaves_short_text_alone_and_counts_characters_not_bytes() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("日本語です", 3), "日本語…");
    }
}
