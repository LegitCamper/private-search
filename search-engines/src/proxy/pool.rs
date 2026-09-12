use super::{Route, RouteId, env_duration};
use crate::{EngineError, https_only_builder};
use reqwest::{Client, Proxy, Url};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(crate) struct Entry {
    pub url: String,
    pub latency: Duration,
}

static RANKED: OnceLock<RwLock<Arc<[Entry]>>> = OnceLock::new();
static CURSOR: OnceLock<Mutex<usize>> = OnceLock::new();
static BLOCKED: OnceLock<Mutex<HashMap<&'static str, HashMap<String, Instant>>>> = OnceLock::new();
static CLIENTS: OnceLock<Mutex<VecDeque<(String, Client)>>> = OnceLock::new();

fn ranked() -> &'static RwLock<Arc<[Entry]>> {
    RANKED.get_or_init(|| RwLock::new(Arc::from([])))
}

pub(crate) fn snapshot() -> Arc<[Entry]> {
    ranked().read().unwrap().clone()
}

pub(crate) fn replace_ranked(mut entries: Vec<Entry>) {
    entries.sort_by_key(|entry| entry.latency);
    *ranked().write().unwrap() = entries.into();
}

pub(crate) fn validate_proxy(value: &str) -> Result<String, String> {
    let url = Url::parse(value).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("proxy scheme must be http or https".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("proxy credentials, paths, queries, and fragments are not allowed".into());
    }
    let ip: IpAddr = url
        .host_str()
        .ok_or("proxy host is missing")?
        .parse()
        .map_err(|_| "proxy host must be a literal IP")?;
    if !is_public(ip) {
        return Err("proxy IP is not globally routable".into());
    }
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    let explicit_port = authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok());
    if explicit_port.is_none() || explicit_port == Some(0) {
        return Err("valid proxy port is required".into());
    }
    let host = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    Ok(format!(
        "{}://{host}:{}",
        url.scheme(),
        explicit_port.unwrap()
    ))
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || a == 0
                || a >= 224
                || a == 100 && (64..=127).contains(&b)
                || a == 192 && b == 0
                || a == 192 && b == 0 && c == 2
                || a == 192 && b == 88 && c == 99
                || a == 198 && matches!(b, 18 | 19 | 51)
                || a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| !is_public(IpAddr::V4(mapped)))
                || segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

fn build_client(url: &str) -> Result<Client, EngineError> {
    let proxy = Proxy::all(url).map_err(EngineError::ReqwestError)?;
    https_only_builder()
        .proxy(proxy)
        .timeout(env_duration("ENGINE_PROXY_REQUEST_TIMEOUT_SECS", 2))
        .pool_max_idle_per_host(1)
        .build()
        .map_err(EngineError::ReqwestError)
}

pub(crate) fn direct_route() -> Route {
    Route {
        id: RouteId::Direct,
        client: super::super::browser_client(),
    }
}

fn proxy_route(url: &str) -> Option<Route> {
    let cap = super::env_usize("ENGINE_PROXY_CLIENT_CACHE", 64).clamp(1, 512);
    let mut cache = CLIENTS
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .ok()?;
    if let Some(index) = cache.iter().position(|(key, _)| key == url) {
        let (_, client) = cache.remove(index)?;
        cache.push_front((url.to_owned(), client.clone()));
        return Some(Route {
            id: RouteId::Proxy(url.to_owned()),
            client,
        });
    }
    let client = build_client(url).ok()?;
    cache.push_front((url.to_owned(), client.clone()));
    cache.truncate(cap);
    Some(Route {
        id: RouteId::Proxy(url.to_owned()),
        client,
    })
}

fn blocked(engine: &'static str, url: &str) -> bool {
    let now = Instant::now();
    let mut all = BLOCKED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let Some(engine_blocks) = all.get_mut(engine) else {
        return false;
    };
    engine_blocks.retain(|_, expiry| *expiry > now);
    engine_blocks.contains_key(url)
}

pub(crate) fn block(engine: &'static str, id: &RouteId) {
    let RouteId::Proxy(url) = id else { return };
    let expiry = Instant::now() + env_duration("ENGINE_PROXY_BLOCK_TTL_SECS", 1800);
    let mut all = BLOCKED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let engine_blocks = all.entry(engine).or_default();
    engine_blocks.retain(|_, expiry| *expiry > Instant::now());
    engine_blocks.insert(url.clone(), expiry);
}

pub(crate) fn penalize(id: &RouteId) {
    let RouteId::Proxy(url) = id else { return };
    let mut guard = ranked().write().unwrap();
    let entries: Vec<_> = guard
        .iter()
        .filter(|entry| entry.url != *url)
        .cloned()
        .collect();
    *guard = entries.into();
}

pub(crate) fn route_for_id(id: &RouteId, engine: &'static str) -> Option<Route> {
    match id {
        RouteId::Direct => Some(direct_route()),
        RouteId::Proxy(url)
            if !blocked(engine, url) && snapshot().iter().any(|e| e.url == *url) =>
        {
            proxy_route(url)
        }
        RouteId::Proxy(_) => None,
    }
}

pub(crate) fn next_routes(
    engine: &'static str,
    count: usize,
    tried: &HashSet<RouteId>,
) -> Vec<Route> {
    let entries = snapshot();
    let top_k = super::env_usize("ENGINE_PROXY_TOP_K", 25).clamp(1, entries.len().max(1));
    let candidates = &entries[..entries.len().min(top_k)];
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut cursor = CURSOR.get_or_init(|| Mutex::new(0)).lock().unwrap();
    let start = *cursor % candidates.len();
    *cursor = (*cursor + count) % candidates.len();
    (0..candidates.len())
        .map(|offset| &candidates[(start + offset) % candidates.len()])
        .filter(|entry| !blocked(engine, &entry.url))
        .filter(|entry| !tried.contains(&RouteId::Proxy(entry.url.clone())))
        .filter_map(|entry| proxy_route(&entry.url))
        .take(count)
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn rejects_internal_and_non_ip_proxy_addresses() {
        for value in [
            "http://127.0.0.1:8080",
            "http://[IP_9b9c2e733bcb]:8080",
            "http://example.com:8080",
            "socks5://192.0.2.1:1080",
            "http://8.8.8.8:8080/?query",
            "http://8.8.8.8:8080/#fragment",
            "http://8.8.8.8:0",
        ] {
            assert!(validate_proxy(value).is_err(), "accepted {value}");
        }
        assert!(validate_proxy("http://8.8.8.8:8080").is_ok());
    }
}
