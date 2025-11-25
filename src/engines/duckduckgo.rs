use chrono::{NaiveDateTime, Utc};
use percent_encoding::percent_decode;
use regex::Regex;
use reqwest::{Client, StatusCode, Url};
use scraper::{ElementRef, Html, Selector};
use std::{collections::HashMap, time::Duration};

use crate::engines::{Engine, EngineError, EngineState, Engines, cache::ResultRow};

#[derive(Debug)]
pub enum Error {
    VqdUnknown,
}

const TIMEOUT: u64 = 2; // seconds
const TIMEOUT_LEN: u64 = 60; // time (min) before engine is available again
const COOLDOWN: u64 = 2; // seconds

pub struct DuckDuckGo {
    vqd: Option<String>,
    state: EngineState,
    cooldown: Option<NaiveDateTime>,
}

impl Engine for DuckDuckGo {
    type Error = Error;

    fn name(&self) -> Engines {
        Engines::DuckDuckGo
    }

    fn is_available(&mut self) -> bool {
        let now = Utc::now().naive_utc();

        if let EngineState::TimedOut { available_at } = self.state {
            if now >= available_at {
                self.state = EngineState::Healthy;
            }
        }

        if let Some(available_at) = self.cooldown {
            if now >= available_at {
                self.cooldown = None;
            }
        }

        matches!(self.state, EngineState::Healthy) && self.cooldown.is_none()
    }

    async fn search(
        &mut self,
        query: &str,
        start: usize,
        need: usize,
    ) -> Result<Vec<ResultRow>, EngineError<Error>> {
        let want_total = start + need;
        let mut results: Vec<ResultRow> = Vec::new();
        let mut page_results = 0;
        let client = Client::builder()
            .timeout(Duration::from_secs(TIMEOUT))
            .build()
            .map_err(EngineError::ReqwestError)?;

        let vqd = self.get_vqd(query).await?;

        while results.len() < want_total {
            let s = page_results.to_string();
            let mut form = HashMap::new();
            form.insert("q", query);
            form.insert("kl", "us-en");
            form.insert("s", &s);
            form.insert("vqd", &vqd);

            let req = client
                .post("https://html.duckduckgo.com/html/")
                .form(&form)
                .send()
                .await;

            let resp = match req {
                Ok(resp) => {
                    self.cooldown = Some((Utc::now() + Duration::from_secs(COOLDOWN)).naive_utc());
                    resp
                }
                Err(e) => {
                    if e.is_timeout() {
                        self.state = EngineState::TimedOut {
                            available_at: (Utc::now() + Duration::from_mins(TIMEOUT_LEN))
                                .naive_utc(),
                        };
                        return Err(EngineError::Timeout);
                    } else {
                        return Err(EngineError::ReqwestError(e));
                    }
                }
            };

            let html = resp.text().await.map_err(EngineError::ReqwestError)?;

            page_results += Self::parse(&mut results, &html)?;
        }

        let end = (start + need).min(results.len());
        let slice = results[start..end].to_vec();

        Ok(slice)
    }
}

impl DuckDuckGo {
    pub fn new() -> Self {
        Self {
            vqd: None,
            state: EngineState::Healthy,
            cooldown: None,
        }
    }

    async fn get_vqd(&mut self, query: &str) -> Result<String, EngineError<Error>> {
        if let Some(v) = &self.vqd {
            return Ok(v.clone());
        } else {
            let resp = reqwest::get(&format!("https://duckduckgo.com/q?={}", query))
                .await
                .map_err(EngineError::ReqwestError)?;

            if resp.status() == StatusCode::OK {
                let html = resp.text().await.map_err(EngineError::ReqwestError)?;
                match Self::extract_vqd(&html) {
                    Some(new_vqd) => {
                        self.vqd = Some(new_vqd.clone());
                        return Ok(new_vqd);
                    }
                    None => return Err(EngineError::EngineSpecificError(Error::VqdUnknown)),
                }
            }
        }
        Err(EngineError::EngineSpecificError(Error::VqdUnknown))
    }

    fn extract_vqd(script_html: &str) -> Option<String> {
        let re = Regex::new(r#"vqd\s*=\s*"([0-9-]+)""#).ok()?;
        let caps = re.captures(script_html)?;
        Some(caps[1].to_string())
    }

    fn parse(results: &mut Vec<ResultRow>, document: &str) -> Result<usize, EngineError<Error>> {
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
                results.push(ResultRow {
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
        if ddg_href.contains("?ad_domain") || ddg_href.contains("?ad_provider") {
            return true;
        }
        false
    }
}
