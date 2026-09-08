//! GitLab public project search, for the "I want the *project* named X, not
//! a blog post about it" case — `veloren` should return
//! `gitlab.com/veloren/veloren`, not a review of it.
//!
//! Uses the unauthenticated REST API (`/api/v4/projects?search=`), which
//! serves public projects to anonymous callers at 500 requests/minute per IP.
//! Three quirks worth knowing:
//!
//! * GitLab omits `X-Total` / `X-Total-Pages` for unauthenticated search — the
//!   count is too expensive to compute — so there is no result total to page
//!   against. Exhaustion is detected the way the cache layer expects instead:
//!   a page that comes back empty yields an empty `Vec`. (`X-Next-Page` and
//!   `X-Per-Page` *are* present, but `X-Next-Page` keeps pointing at a next
//!   page right up to the last one, so it tells us nothing extra.)
//! * Offset pagination is hard-capped at 50 000 records for projects; past
//!   that the API answers HTTP 405 with a JSON error telling you to switch to
//!   keyset pagination. We stop short of the cap and report exhaustion.
//! * A large share of public projects have `"description": null`, which would
//!   violate the non-empty-description contract. Those get a description
//!   synthesized from the metadata GitLab does return (namespace, topics,
//!   stars, forks, last activity).

use async_trait::async_trait;
use serde::Deserialize;

use super::{encode, get_html, page_number, tidy, truncate};
use crate::{EngineError, EngineInfo, RawResult, SearchEngine};

#[derive(Clone)]
pub struct GitLab;

impl EngineInfo for GitLab {
    fn name(&self) -> &'static str {
        "GitLab"
    }
}

/// Pinned rather than taken from the caller's `count` hint: `start` is
/// translated to a page number, so the page size has to stay constant across
/// calls or the arithmetic silently skips or repeats results.
const PER_PAGE: usize = 20;

/// GitLab refuses offset pagination beyond this many records for `Project`
/// ("Offset pagination has a maximum allowed offset of 50000", HTTP 405).
/// Reaching it means the caller has exhausted this engine, not that anything
/// broke, so we report it as no-more-results rather than letting the 405
/// surface as a parse failure.
const MAX_OFFSET: usize = 50_000;

/// Hard ceiling on a result description, per the engine contract.
const DESCRIPTION_MAX: usize = 300;

/// Budget for the project's own blurb, leaving room for the " — 2388 stars ·
/// 815 forks · updated 2026-09-07" trailer so the enrichment survives
/// truncation of a long upstream description.
const BLURB_MAX: usize = 220;

fn build_search_url(query: &str, start: usize) -> String {
    let page = page_number(start, PER_PAGE);
    format!(
        "https://gitlab.com/api/v4/projects?search={}\
         &order_by=star_count&sort=desc&per_page={PER_PAGE}&page={page}",
        encode(query)
    )
}

/// One entry of the `/projects` array. Every field is optional because the
/// anonymous view of a project omits some of them and `description` is
/// routinely `null`; a missing field should degrade one result, not fail the
/// whole page.
#[derive(Deserialize)]
struct Project {
    web_url: Option<String>,
    path_with_namespace: Option<String>,
    name_with_namespace: Option<String>,
    name: Option<String>,
    description: Option<String>,
    star_count: Option<u64>,
    forks_count: Option<u64>,
    last_activity_at: Option<String>,
    #[serde(default)]
    topics: Vec<String>,
}

/// `path_with_namespace` ("veloren/veloren") is preferred over
/// `name_with_namespace` ("Veloren / veloren") because it matches the URL the
/// user is about to click.
fn title_of(project: &Project) -> Option<String> {
    [
        &project.path_with_namespace,
        &project.name_with_namespace,
        &project.name,
    ]
    .into_iter()
    .flatten()
    .map(|candidate| tidy(candidate))
    .find(|candidate| !candidate.is_empty())
}

/// Stars, forks and last activity, always non-empty — this is what keeps a
/// `null`-description project's description from being blank.
fn stats(project: &Project) -> String {
    let stars = project.star_count.unwrap_or(0);
    let forks = project.forks_count.unwrap_or(0);
    let mut parts = vec![
        format!("{stars} star{}", if stars == 1 { "" } else { "s" }),
        format!("{forks} fork{}", if forks == 1 { "" } else { "s" }),
    ];

    // `last_activity_at` is an ISO-8601 instant; the date alone is the useful
    // part in a one-line result row.
    if let Some(day) = project
        .last_activity_at
        .as_deref()
        .and_then(|timestamp| timestamp.split('T').next())
        .map(str::trim)
        .filter(|day| !day.is_empty())
    {
        parts.push(format!("updated {day}"));
    }

    parts.join(" · ")
}

fn describe(project: &Project, title: &str) -> String {
    let blurb = project.description.as_deref().map(tidy).unwrap_or_default();

    let mut described = if blurb.is_empty() {
        let topics = project
            .topics
            .iter()
            .map(|topic| tidy(topic))
            .filter(|topic| !topic.is_empty())
            .collect::<Vec<_>>();

        if topics.is_empty() {
            format!("GitLab project {title}")
        } else {
            format!("GitLab project {title}. Topics: {}", topics.join(", "))
        }
    } else {
        truncate(&blurb, BLURB_MAX)
    };

    described.push_str(" — ");
    described.push_str(&stats(project));
    truncate(&described, DESCRIPTION_MAX)
}

fn parse_response(json: &str) -> Result<Vec<RawResult>, EngineError> {
    let projects: Vec<Project> = serde_json::from_str(json).map_err(|e| {
        EngineError::ParseError(format!(
            "GitLab returned a body that isn't the project array we expect ({e}); \
             the API shape may have changed"
        ))
    })?;

    Ok(projects
        .iter()
        .filter_map(|project| {
            // `web_url` is the human-facing project page. Anything that isn't
            // an absolute https URL is unopenable, so drop the row rather than
            // hand the caller a broken link.
            let url = project
                .web_url
                .as_deref()
                .map(str::trim)
                .filter(|url| url.starts_with("https://"))?;
            let title = title_of(project)?;
            let description = describe(project, &title);

            Some(RawResult {
                url: url.to_string(),
                title,
                description,
            })
        })
        .collect())
}

#[async_trait]
impl SearchEngine for GitLab {
    /// `count` is ignored: the page size is pinned by [`PER_PAGE`] (see there).
    async fn search_results(
        &self,
        query: &str,
        start: usize,
        _count: usize,
    ) -> Result<Vec<RawResult>, EngineError> {
        if start >= MAX_OFFSET {
            return Ok(Vec::new());
        }

        // `get_html` rather than `get_json` because the pure `parse_response`
        // needs the raw body; GitLab's API ignores the browser client's
        // `Accept: text/html` and answers `application/json` regardless.
        let body = get_html(&build_search_url(query, start), "GitLab").await?;
        parse_response(&body)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sites::fixture;

    #[test]
    fn build_search_url_requests_the_first_page_for_a_fresh_query() {
        assert_eq!(
            build_search_url("rust", 0),
            "https://gitlab.com/api/v4/projects?search=rust\
             &order_by=star_count&sort=desc&per_page=20&page=1"
        );
    }

    #[test]
    fn build_search_url_advances_the_page_as_the_caller_accumulates_results() {
        // Still page 1 until the caller actually holds a full page.
        assert_eq!(
            build_search_url("rust", PER_PAGE - 1),
            "https://gitlab.com/api/v4/projects?search=rust\
             &order_by=star_count&sort=desc&per_page=20&page=1"
        );
        assert_eq!(
            build_search_url("rust", PER_PAGE),
            "https://gitlab.com/api/v4/projects?search=rust\
             &order_by=star_count&sort=desc&per_page=20&page=2"
        );
        assert_eq!(
            build_search_url("rust", PER_PAGE * 4 + 3),
            "https://gitlab.com/api/v4/projects?search=rust\
             &order_by=star_count&sort=desc&per_page=20&page=5"
        );
    }

    #[test]
    fn build_search_url_percent_encodes_spaces_and_non_ascii() {
        assert_eq!(
            build_search_url("café tooling", 0),
            "https://gitlab.com/api/v4/projects?search=caf%C3%A9%20tooling\
             &order_by=star_count&sort=desc&per_page=20&page=1"
        );
    }

    #[test]
    fn parse_response_reads_a_full_page_of_projects_from_the_real_fixture() {
        let results = parse_response(&fixture("gitlab.json")).unwrap();

        assert_eq!(results.len(), 20);
        assert!(
            results
                .iter()
                .all(|r| !r.url.is_empty() && !r.title.is_empty() && !r.description.is_empty()),
            "every field is contractually non-empty; a null description must \
             have been synthesized, not passed through"
        );
        assert!(
            results
                .iter()
                .all(|r| r.url.starts_with("https://gitlab.com/")),
            "results must link to the human-facing page, never the API"
        );
        assert!(
            results
                .iter()
                // `+ 1` for the ellipsis `truncate` appends when it cuts.
                .all(|r| r.description.chars().count() <= DESCRIPTION_MAX + 1)
        );
    }

    #[test]
    fn parse_response_first_result_matches_the_real_fixture() {
        let results = parse_response(&fixture("gitlab.json")).unwrap();

        assert_eq!(results[0].url, "https://gitlab.com/veloren/veloren");
        assert_eq!(results[0].title, "veloren/veloren");
        assert!(
            results[0].description.contains("2388 stars"),
            "the star count enrichment should be appended: {}",
            results[0].description
        );
    }

    /// An empty page is how this engine signals exhaustion, so it has to parse
    /// cleanly — an error here would be reported as an engine failure instead.
    #[test]
    fn parse_response_treats_an_empty_array_as_no_more_results() {
        assert_eq!(parse_response("[]").unwrap().len(), 0);
    }

    /// Trimmed from a real page-100 response; GitLab returns `null`
    /// descriptions constantly and the contract forbids an empty one.
    const NULL_DESCRIPTION_PROJECT: &str = r#"[
        {
            "id": 85465265,
            "description": null,
            "name": "sinus-specialist",
            "name_with_namespace": "Muhammad  Faizan / sinus-specialist",
            "path_with_namespace": "MuhammadFaizan12221/sinus-specialist",
            "web_url": "https://gitlab.com/MuhammadFaizan12221/sinus-specialist",
            "topics": ["health", "rust"],
            "forks_count": 1,
            "star_count": 0,
            "last_activity_at": "2026-08-17T05:25:49.038Z"
        }
    ]"#;

    #[test]
    fn parse_response_synthesizes_a_description_for_a_project_that_has_none() {
        let results = parse_response(NULL_DESCRIPTION_PROJECT).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].description,
            "GitLab project MuhammadFaizan12221/sinus-specialist. \
             Topics: health, rust — 0 stars · 1 fork · updated 2026-08-17"
        );
    }

    #[test]
    fn parse_response_rejects_a_body_that_is_not_a_project_array() {
        assert!(matches!(
            parse_response(r#"{"error":"401 Unauthorized"}"#),
            Err(EngineError::ParseError(_))
        ));
    }

    /// No network: the cap is checked before the request is built.
    #[tokio::test]
    async fn search_results_reports_exhaustion_past_gitlabs_offset_cap() {
        let results = GitLab
            .search_results("rust", MAX_OFFSET, PER_PAGE)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[ignore]
    #[tokio::test]
    async fn gitlab_live_search_returns_openable_projects() {
        let results = GitLab.search_results("rust", 0, PER_PAGE).await.unwrap();

        assert!(!results.is_empty());
        assert!(results.iter().all(|r| r.url.starts_with("https://")
            && !r.title.is_empty()
            && !r.description.is_empty()));
    }

    #[ignore]
    #[tokio::test]
    async fn gitlab_live_pagination_does_not_repeat_the_first_page() {
        let page1 = GitLab.search_results("rust", 0, PER_PAGE).await.unwrap();
        let page2 = GitLab
            .search_results("rust", PER_PAGE, PER_PAGE)
            .await
            .unwrap();

        assert!(!page1.is_empty() && !page2.is_empty());
        assert!(
            page2
                .iter()
                .all(|r2| !page1.iter().any(|r1| r1.url == r2.url))
        );
    }
}
