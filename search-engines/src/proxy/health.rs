use super::{env_duration, pool};
use futures::{StreamExt, stream};
use serde::Serialize;
use std::{collections::HashSet, net::IpAddr, time::Instant};

const DEFAULT_SOURCE: &str =
    "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/http.txt";
const DEFAULT_ECHO: &str = "https://api.ipify.org";
const MAX_LIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_ECHO_BYTES: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStats {
    pub healthy: usize,
    pub fastest_ms: Option<u128>,
    pub slowest_ms: Option<u128>,
}

pub fn stats() -> ProxyStats {
    let entries = pool::snapshot();
    ProxyStats {
        healthy: entries.len(),
        fastest_ms: entries.first().map(|entry| entry.latency.as_millis()),
        slowest_ms: entries.last().map(|entry| entry.latency.as_millis()),
    }
}

fn parse_list(body: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let value = if line.contains("://") {
                line.to_owned()
            } else {
                format!("http://{line}")
            };
            pool::validate_proxy(&value).ok()
        })
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

async fn capped_text(response: reqwest::Response, cap: usize) -> Result<String, String> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(format!("response exceeds {cap} byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| "response is not UTF-8".into())
}

async fn echo_ip(client: &reqwest::Client, echo: &str) -> Result<IpAddr, String> {
    let response = client
        .get(echo)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|error| error.to_string())?;
    capped_text(response, MAX_ECHO_BYTES)
        .await?
        .trim()
        .parse()
        .map_err(|_| "IP echo returned an invalid address".into())
}

pub async fn refresh_once() -> Result<ProxyStats, String> {
    let source = std::env::var("ENGINE_PROXY_SOURCE_URL").unwrap_or_else(|_| DEFAULT_SOURCE.into());
    if reqwest::Url::parse(&source)
        .map_err(|e| e.to_string())?
        .scheme()
        != "https"
    {
        return Err("proxy source must use HTTPS".into());
    }
    let echo = std::env::var("ENGINE_PROXY_ECHO_URL").unwrap_or_else(|_| DEFAULT_ECHO.into());
    if reqwest::Url::parse(&echo)
        .map_err(|e| e.to_string())?
        .scheme()
        != "https"
    {
        return Err("proxy probe target must use HTTPS".into());
    }

    let timeout = env_duration("ENGINE_PROXY_PROBE_TIMEOUT_SECS", 5);
    let direct = crate::https_only_builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let direct_ip = echo_ip(&direct, &echo).await?;
    let response = direct
        .get(source)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|e| e.to_string())?;
    let body = capped_text(response, MAX_LIST_BYTES).await?;

    let concurrency = super::env_usize("ENGINE_PROXY_PROBE_CONCURRENCY", 50).clamp(1, 200);
    let candidate_cap = super::env_usize("ENGINE_PROXY_PROBE_CANDIDATES", 400).clamp(1, 3000);
    let previous: Vec<_> = pool::snapshot()
        .iter()
        .map(|entry| entry.url.clone())
        .collect();
    let previous_set: HashSet<_> = previous.iter().cloned().collect();
    let candidates = previous
        .into_iter()
        .chain(
            parse_list(&body)
                .into_iter()
                .filter(|url| !previous_set.contains(url)),
        )
        .take(candidate_cap);

    let entries = stream::iter(candidates)
        .map(|url| {
            let echo = echo.clone();
            async move {
                let client = crate::https_only_builder()
                    .proxy(reqwest::Proxy::all(&url).ok()?)
                    .timeout(timeout)
                    .pool_max_idle_per_host(0)
                    .build()
                    .ok()?;
                let started = Instant::now();
                let exit_ip = echo_ip(&client, &echo).await.ok()?;
                (exit_ip != direct_ip).then_some(pool::Entry {
                    url,
                    latency: started.elapsed(),
                })
            }
        })
        .buffer_unordered(concurrency)
        .filter_map(|entry| async move { entry })
        .collect::<Vec<_>>()
        .await;
    pool::replace_ranked(entries);
    Ok(stats())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn list_parser_accepts_only_unique_public_ip_ports() {
        let parsed = parse_list("8.8.8.8:80\n8.8.8.8:80\n127.0.0.1:80\nexample.com:80\n");
        assert_eq!(parsed, vec!["http://8.8.8.8:80"]);
    }
}
