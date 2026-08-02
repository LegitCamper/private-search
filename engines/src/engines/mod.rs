use async_trait::async_trait;
use rand::seq::IndexedRandom;
use reqwest::Client;
use scraper::{Html, Selector};

use crate::cache::{self, ImagesRow, ResultRow};

mod brave;
mod duckduckgo;

pub use brave::Brave;
pub use duckduckgo::DuckDuckGo;

#[derive(Debug)]
pub enum EngineError {
    ReqwestError(reqwest::Error),
    ParseError(String),
    Timeout, // engine timeout
}

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
    ) -> Result<Vec<ResultRow>, EngineError>;
}

#[async_trait]
pub trait ImageEngine: EngineInfo + Clone + Send {
    /// See [`SearchEngine::search_results`] — same paging contract.
    async fn search_images(
        &self,
        query: &str,
        start: usize,
        count: usize,
    ) -> Result<Vec<ImagesRow>, EngineError>;
}

fn new_rand_client() -> Result<Client, reqwest::Error> {
    static USER_AGENTS: &[&str] = &[
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "Mozilla/5.0 (X11; Linux x86_64; rv:118.0) Gecko/20100101 Firefox/118.0",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 13_4) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/118.0.5993.72 Safari/537.36",
    ];

    let user_agent = USER_AGENTS.choose(&mut rand::rng()).unwrap();

    Client::builder().user_agent(*user_agent).build()
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

        if !refresh
            && let Ok(html) = std::fs::read_to_string(&path)
        {
            return html;
        }

        let html = super::new_rand_client()
            .expect("failed to build http client")
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
) -> Vec<ResultRow> {
    let html = Html::parse_document(html);

    let results_selector = Selector::parse(results_selector).expect(PARSE_ERROR);
    let title_selector = Selector::parse(title_selector).expect(PARSE_ERROR);
    let href_selector = Selector::parse(href_selector).expect(PARSE_ERROR);
    let description_selector = Selector::parse(description_selector).expect(PARSE_ERROR);

    let mut results = Vec::new();

    for result in html.select(&results_selector) {
        results.push(ResultRow {
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
) -> Vec<ImagesRow> {
    let html = Html::parse_document(html);

    let images_selector = Selector::parse(images_selector).expect(PARSE_ERROR);
    let title_selector = Selector::parse(title_selector).expect(PARSE_ERROR);
    let img_selector = Selector::parse(img_selector).expect(PARSE_ERROR);

    let mut images = Vec::new();

    for result in html.select(&images_selector) {
        images.push(ImagesRow {
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
}
