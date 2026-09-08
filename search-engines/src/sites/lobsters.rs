//! Lobsters (<https://lobste.rs>) — a small, invite-only programming link
//! aggregator. Its signal-to-noise ratio is high enough that a Lobsters hit
//! for a topic is usually worth more than a page of general web results, so
//! this engine exists to surface "what has Lobsters discussed about X".
//!
//! Quirk: unlike most of the sites in this module, Lobsters has **no usable
//! JSON search API**. Lobsters serves `.json` on many routes (`/`, `/s/<id>`,
//! `/t/<tag>`), but `/search.json` is not one of them — it answers
//! `400 {"error":"400 Unpermitted query or form parameter"}` for *every*
//! request, even with no query parameters at all, because the strict
//! parameter filter on that action doesn't permit Rails' implicit `format`
//! param. So this engine scrapes the HTML search page.
//!
//! Lobsters is volunteer-run on modest hardware, so this client is
//! deliberately conservative: one request per page, a fixed page size, and no
//! retries. Don't add a retry loop here.

use async_trait::async_trait;
use scraper::{ElementRef, Html, Selector};

use super::{encode, get_html, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct Lobsters;

impl EngineInfo for Lobsters {
    fn name(&self) -> &'static str {
        "Lobsters"
    }
}

/// Stories per search page. Fixed by the server — there is no per-page
/// parameter — so the caller's `count` hint is ignored.
const PER_PAGE: usize = 20;

const BASE: &str = "https://lobste.rs";

fn build_search_url(query: &str, start: usize) -> String {
    // `what=stories` excludes comment hits (whose "title" is a comment
    // fragment, not a story), and `order=relevance` is what a search engine
    // wants; the site's own default is newest-first.
    format!(
        "{BASE}/search?q={}&what=stories&order=relevance&page={}",
        encode(query),
        page_number(start, PER_PAGE)
    )
}

// The results box is rendered even for a zero-hit query (with an empty
// `<ol class="stories list">`), so its absence means we got something other
// than a search page — a block, an error page, or a redesign. Without this,
// any of those would silently look like genuine exhaustion.
fn looks_like_search_results(html: &str) -> bool {
    html.contains(r#"class="box searchresults""#)
}

fn selector(css: &'static str) -> Selector {
    Selector::parse(css).expect("static selector should be valid CSS")
}

/// Text of the first descendant matching `css` that actually has any,
/// whitespace-collapsed. Skipping the empty ones matters: the byline's first
/// `/~user` link wraps only the submitter's avatar `<img>`, so taking
/// literally the first match would yield an empty string.
fn text_of(element: &ElementRef, css: &'static str) -> Option<String> {
    element
        .select(&selector(css))
        .map(|e| tidy(&e.text().collect::<String>()))
        .find(|s| !s.is_empty())
}

fn attr_of(element: &ElementRef, css: &'static str, attr: &str) -> Option<String> {
    element
        .select(&selector(css))
        .next()
        .and_then(|e| e.value().attr(attr))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn absolute(href: &str) -> Option<String> {
    if href.starts_with("https://") {
        Some(href.to_string())
    } else if let Some(rest) = href.strip_prefix("http://") {
        // Upgrade the http:// submissions from the site's older years.
        Some(format!("https://{rest}"))
    } else if href.starts_with('/') {
        Some(format!("{BASE}{href}"))
    } else {
        None
    }
}

pub fn parse_response(body: &str) -> Result<Vec<RawResult>, EngineError> {
    let document = Html::parse_document(body);
    let mut results = Vec::new();

    for story in document.select(&selector("li.story")) {
        let Some(title) = text_of(&story, "a.u-url") else {
            continue;
        };

        // The story's own Lobsters page, not the submitted link. The
        // discussion is the part of a story that only Lobsters has, the
        // comments page shows the target link at the top (so the user loses
        // nothing by going through it), and it keeps this engine from
        // returning the same URL a general web engine already found. The
        // target link goes in the description instead.
        let comments_path = attr_of(&story, ".comments_label a[href]", "href").or_else(|| {
            story
                .value()
                .attr("data-shortid")
                .map(|id| format!("/s/{id}"))
        });
        let Some(url) = comments_path.as_deref().and_then(absolute) else {
            continue;
        };

        // Self ("text") posts point `a.u-url` at their own comments page, in
        // which case there is no offsite link to advertise.
        let target = attr_of(&story, "a.u-url", "href")
            .filter(|href| !href.starts_with('/'))
            .and_then(|href| absolute(&href));

        let mut parts = Vec::new();
        if let Some(score) = text_of(&story, ".voters .upvoter") {
            parts.push(format!("{score} points"));
        }
        // Reads "no comments" / "1 comment" / "12 comments" verbatim.
        if let Some(comments) = text_of(&story, ".comments_label a") {
            parts.push(comments);
        }
        let tags: Vec<String> = story
            .select(&selector(".tags .tag"))
            .map(|t| tidy(&t.text().collect::<String>()))
            .filter(|t| !t.is_empty())
            .collect();
        if !tags.is_empty() {
            parts.push(format!("tagged {}", tags.join(", ")));
        }
        if let Some(submitter) = text_of(&story, ".byline a[href^='/~']") {
            parts.push(format!("via {submitter}"));
        }
        match &target {
            Some(link) => parts.push(format!("links to {link}")),
            None => parts.push("Lobsters text post".to_string()),
        }

        let description = truncate(&tidy(&parts.join(" · ")), 300);
        if description.is_empty() {
            continue;
        }

        results.push(RawResult {
            url,
            title,
            description,
        });
    }

    Ok(results)
}

#[async_trait]
impl SearchEngine for Lobsters {
    /// `count` is ignored: the search page's size is fixed server-side.
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        let html = get_html(&build_search_url(query, start), "Lobsters").await?;
        if !looks_like_search_results(&html) {
            return Err(EngineError::ParseError(
                "Lobsters response markup may have changed; results marker was missing".into(),
            ));
        }

        parse_response(&html)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_asks_for_the_first_page_of_story_hits() {
        assert_eq!(
            build_search_url("rust", 0),
            "https://lobste.rs/search?q=rust&what=stories&order=relevance&page=1"
        );
    }

    #[test]
    fn build_search_url_translates_an_offset_into_a_one_based_page() {
        assert_eq!(
            build_search_url("rust", PER_PAGE),
            "https://lobste.rs/search?q=rust&what=stories&order=relevance&page=2"
        );
        assert_eq!(
            build_search_url("rust", PER_PAGE * 2 + 7),
            "https://lobste.rs/search?q=rust&what=stories&order=relevance&page=3"
        );
    }

    #[test]
    fn build_search_url_escapes_spaces_and_non_ascii() {
        assert_eq!(
            build_search_url("café async", 0),
            "https://lobste.rs/search?q=caf%C3%A9%20async&what=stories&order=relevance&page=1"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_of_stories_from_the_real_fixture() {
        let results = parse_response(&fixture("lobsters.html")).unwrap();

        assert_eq!(results.len(), PER_PAGE);
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every field is part of the contract; a stale selector shows up \
             here as an empty string rather than a missing result"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://lobste.rs/s/"))
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_recorded_fixture() {
        let results = parse_response(&fixture("lobsters.html")).unwrap();

        assert_eq!(
            results[0].url,
            "https://lobste.rs/s/mzkw9y/rewrite_optimize_repeat_our_journey"
        );
        assert_eq!(
            results[0].title,
            "Rewrite, Optimize, Repeat: Our Journey Porting a Triemap from C to Rust"
        );
        assert!(
            results[0]
                .description
                .contains("https://www.youtube.com/watch?v=H0AUP2OgppE"),
            "the submitted link belongs in the description: {}",
            results[0].description
        );
        assert!(
            results[0]
                .description
                .contains("tagged video, c, performance, rust")
        );
        assert!(results[0].description.contains("via ohrv"));
    }

    // Mirrors the real zero-hit page: the results box is present, the story
    // list is just empty. That's exhaustion, not a failure.
    #[test]
    fn parse_response_treats_a_well_formed_empty_page_as_no_more_results() {
        let html = r#"
            <div class="box searchresults">
                <summary><span class="heading">0 results for </span></summary>
            </div>
            <ol class="stories list"></ol>
        "#;

        assert!(looks_like_search_results(html));
        assert!(parse_response(html).unwrap().is_empty());
    }

    #[test]
    fn parse_response_keeps_self_posts_whose_link_is_their_own_comments_page() {
        let html = r#"
            <li id="story_jyl8yr" data-shortid="jyl8yr" class="story">
              <div class="story_liner h-entry">
                <div class="voters"><a class="upvoter" href="/login">42</a></div>
                <div class="details">
                  <span class="link h-cite u-repost-of">
                    <a class="u-url" href="/s/jyl8yr/what_you_should_ask">What you should ask</a>
                  </span>
                  <ul class="tags"><li><a class="tag tag_ask" href="/t/ask">ask</a></li></ul>
                  <div class="byline">
                    <a href="/~alice"><img class="avatar" src="/avatars/alice-16.png"></a>
                    <a href="/~alice">alice</a>
                    <span class="comments_label">
                      <a href="/s/jyl8yr/what_you_should_ask">31 comments</a>
                    </span>
                  </div>
                </div>
              </div>
            </li>
        "#;

        let results = parse_response(html).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].url,
            "https://lobste.rs/s/jyl8yr/what_you_should_ask"
        );
        assert_eq!(
            results[0].description,
            "42 points · 31 comments · tagged ask · via alice · Lobsters text post"
        );
    }

    #[test]
    fn looks_like_search_results_accepts_the_real_fixture_but_rejects_a_block_page() {
        assert!(looks_like_search_results(&fixture("lobsters.html")));
        assert!(!looks_like_search_results(
            "<html><body>Attention Required! Please verify you are human</body></html>"
        ));
    }

    #[ignore]
    #[tokio::test]
    async fn test_lobsters_search_live() {
        let results = Lobsters.search_results("rust", 0, 20).await.unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().all(|r| r.url.starts_with("https://")));
    }
}
