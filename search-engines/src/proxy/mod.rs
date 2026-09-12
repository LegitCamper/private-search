use crate::{BlockKind, EngineError};
use futures::{Future, StreamExt, stream::FuturesUnordered};
use reqwest::Client;
use std::{collections::HashSet, time::Duration};

mod health;
mod pool;

pub use health::{ProxyStats, refresh_once, stats};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RouteId {
    Direct,
    Proxy(String),
}

#[derive(Clone)]
pub(crate) struct Route {
    pub id: RouteId,
    client: Client,
}

impl Route {
    pub fn client(&self) -> &Client {
        &self.client
    }
}

pub(crate) struct Plan<'a> {
    pub engine: &'static str,
    pub target: &'a str,
    pub pinned: Option<RouteId>,
}

pub(crate) struct Won<T> {
    pub route: RouteId,
    pub value: T,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn env_duration(name: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default),
    )
}

pub fn enabled_for(engine: &str) -> bool {
    matches!(std::env::var("ENGINE_PROXY_ENABLED").as_deref(), Ok("1"))
        && std::env::var("ENGINE_PROXY_ENGINES")
            .unwrap_or_else(|_| "Brave,DuckDuckGo".into())
            .split(',')
            .any(|name| name.trim() == engine)
}

pub fn enabled() -> bool {
    matches!(std::env::var("ENGINE_PROXY_ENABLED").as_deref(), Ok("1"))
}

pub(crate) fn pinned_route(id: &RouteId, engine: &'static str) -> Option<Route> {
    pool::route_for_id(id, engine)
}

pub(crate) fn marker_error(engine: &str, route: &RouteId, detail: &str) -> EngineError {
    if matches!(route, RouteId::Proxy(_)) {
        EngineError::Blocked {
            kind: BlockKind::AccessDenied,
            retry_after: None,
            detail: format!("{engine} proxy response missing results marker"),
        }
    } else {
        EngineError::ParseError(detail.into())
    }
}

pub(crate) async fn race<T, F, Fut>(plan: Plan<'_>, attempt: F) -> Result<Won<T>, EngineError>
where
    F: Fn(Route) -> Fut,
    Fut: Future<Output = Result<T, EngineError>>,
{
    if !enabled_for(plan.engine) {
        return attempt(pool::direct_route()).await.map(|value| Won {
            route: RouteId::Direct,
            value,
        });
    }

    let target = reqwest::Url::parse(plan.target)
        .map_err(|error| EngineError::ParseError(format!("invalid engine URL: {error}")))?;
    if target.scheme() != "https" {
        return Err(EngineError::ParseError(
            "refusing to proxy a non-HTTPS target".into(),
        ));
    }

    if let Some(id) = plan.pinned {
        let Some(route) = pool::route_for_id(&id, plan.engine) else {
            return Err(EngineError::Blocked {
                kind: BlockKind::Captcha,
                retry_after: None,
                detail: format!("{} pinned proxy is unavailable", plan.engine),
            });
        };
        return match attempt(route).await {
            Ok(value) => Ok(Won { route: id, value }),
            Err(error @ EngineError::Blocked { .. }) => {
                pool::block(plan.engine, &id);
                Err(error)
            }
            Err(error @ (EngineError::ReqwestError(_) | EngineError::Timeout)) => {
                pool::penalize(&id);
                Err(error)
            }
            Err(error) => Err(error),
        };
    }

    let default_racers = if plan.engine == "DuckDuckGo" { 2 } else { 3 };
    let racers = env_usize("ENGINE_PROXY_RACERS", default_racers).clamp(1, 3);
    let waves = env_usize("ENGINE_PROXY_WAVES", 2).clamp(1, 2);
    let mut tried = HashSet::new();
    let mut proxy_parse_error = None;
    let mut proxies_all_parse = true;

    for _ in 0..waves {
        let routes = pool::next_routes(plan.engine, racers, &tried);
        if routes.is_empty() {
            break;
        }
        tried.extend(routes.iter().map(|route| route.id.clone()));
        let mut pending = routes
            .into_iter()
            .map(|route| {
                let future = attempt(route.clone());
                async move { (route.id, future.await) }
            })
            .collect::<FuturesUnordered<_>>();

        while let Some((route, result)) = pending.next().await {
            match result {
                Ok(value) => return Ok(Won { route, value }),
                Err(error) => match error {
                    error @ EngineError::Blocked { .. } => {
                        proxies_all_parse = false;
                        pool::block(plan.engine, &route);
                        let _ = error;
                    }
                    error @ (EngineError::ReqwestError(_) | EngineError::Timeout) => {
                        proxies_all_parse = false;
                        pool::penalize(&route);
                        let _ = error;
                    }
                    error @ EngineError::ParseError(_) => proxy_parse_error = Some(error),
                },
            }
        }
    }

    match attempt(pool::direct_route()).await {
        Ok(value) => Ok(Won {
            route: RouteId::Direct,
            value,
        }),
        Err(error @ EngineError::ParseError(_)) if proxies_all_parse => {
            Err(proxy_parse_error.unwrap_or(error))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn marker_error_distinguishes_proxy_walls_from_direct_selector_rot() {
        assert!(matches!(
            marker_error(
                "Brave",
                &RouteId::Proxy("http://192.0.2.1:80".into()),
                "missing"
            ),
            EngineError::Blocked { .. }
        ));
        assert!(matches!(
            marker_error("Brave", &RouteId::Direct, "missing"),
            EngineError::ParseError(_)
        ));
    }
}
