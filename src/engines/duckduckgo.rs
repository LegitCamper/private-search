use percent_encoding::percent_decode;
use reqwest::{Client, Url};
use scraper::{ElementRef, Html, Selector};
use std::collections::HashMap;

use crate::engines::{Engine, EngineResult};

#[derive(Debug)]
pub enum Error {
    ReqwestError(reqwest::Error),
    ParseError(String),
}

pub struct DuckDuckGo {
    documents: Vec<String>,
}

impl Engine for DuckDuckGo {
    type Error = Error;

    fn engine() -> super::Engines {
        super::Engines::DuckDuckGo
    }

    async fn search(
        query: &str,
        start: usize,
        need: usize,
    ) -> Result<Vec<EngineResult>, Self::Error> {
        let want_total = start + need;
        let mut results: Vec<EngineResult> = Vec::new();
        let client = Client::new();

        // Get first page first
        let first_html = client
            .get(&format!("https://html.duckduckgo.com/html/?q={}", query))
            .send()
            .await
            .map_err(Error::ReqwestError)?
            .text()
            .await
            .map_err(Error::ReqwestError)?;

        let mut parsed = Self::parse(&mut results, &first_html)?;
        if parsed == 0 {
            return Err(Self::Error::ParseError("first page empty".into()));
        }

        let mut offset = results.len();

        while results.len() < want_total {
            let s = offset.to_string();
            let mut form = HashMap::new();
            form.insert("q", query);
            form.insert("kl", "us-en");
            form.insert("s", &s);

            let html = client
                .post("https://html.duckduckgo.com/html/")
                .form(&form)
                .send()
                .await
                .map_err(Error::ReqwestError)?
                .text()
                .await
                .map_err(Error::ReqwestError)?;

            parsed = Self::parse(&mut results, &html)?;

            // DuckDuckGo sometimes returns nothing on edge offsets
            if parsed == 0 {
                break; // prevent infinite loop
            }

            offset += parsed;
        }

        let end = (start + need).min(results.len());
        let slice = results[start..end].to_vec();

        Ok(slice)
    }
}

impl DuckDuckGo {
    fn parse(results: &mut Vec<EngineResult>, document: &str) -> Result<usize, Error> {
        let mut number_results = 0;

        let links_sel = Selector::parse("#links").unwrap();
        let result_sel = Selector::parse("div.result").unwrap();
        let title_sel = Selector::parse("h2 a").unwrap();
        let url_sel = Selector::parse("a.result__url").unwrap();

        let document = Html::parse_document(&document);

        if let Some(links) = document.select(&links_sel).next() {
            for result in links.select(&result_sel) {
                // Title
                let title = result
                    .select(&title_sel)
                    .next()
                    .map(|t| t.text().collect::<String>())
                    .unwrap_or_default();

                // URL from result__url
                let url = result
                    .select(&url_sel)
                    .next()
                    .and_then(|u| u.value().attr("href"))
                    .map(|href| Self::extract_ddg_url(href).unwrap_or_else(|| href.to_string()))
                    .unwrap_or_default();

                if Self::is_sponsored(&url) {
                    continue;
                }

                let snippet = Self::extract_snippet(&result);

                number_results += 1;
                results.push(EngineResult {
                    url,
                    title,
                    description: snippet,
                });
            }
        }

        Ok(number_results)
    }

    fn collect_text(element: &ElementRef) -> String {
        let mut text = String::new();

        for child in element.children() {
            if let Some(el) = child.value().as_element() {
                // Skip h2 (title) and result__url
                let tag = el.name();
                let classes = el.attr("class").unwrap_or("");
                if tag == "h2" || classes.contains("result__url") {
                    continue;
                }

                if let Some(el_ref) = ElementRef::wrap(child) {
                    let child_text = Self::collect_text(&el_ref);
                    if !child_text.is_empty() {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(&child_text);
                    }
                }
            } else if let Some(t) = child.value().as_text() {
                let t = t.trim();
                if !t.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(t);
                }
            }
        }

        text
    }

    fn extract_snippet(result: &ElementRef) -> String {
        let mut snippet = String::new();

        for child in result.children() {
            if let Some(el) = child.value().as_element() {
                let tag = el.name();
                let classes = el.attr("class").unwrap_or("");
                if tag == "h2" || classes.contains("result__url") {
                    continue; // skip title and url
                }
            }

            if let Some(el_ref) = ElementRef::wrap(child) {
                snippet.push_str(&Self::collect_text(&el_ref));
                snippet.push(' ');
            } else if let Some(text) = child.value().as_text() {
                snippet.push_str(text.trim());
                snippet.push(' ');
            }
        }

        snippet.trim().to_string()
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

    fn is_sponsored(ddg_href: &str) -> bool {
        if ddg_href.contains("?ad_domain") || ddg_href.contains("ad_provider") {
            return true;
        }
        false
    }
}
