use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use rocket::{
    Request,
    http::Status,
    outcome::Outcome,
    request::{self, FromRequest},
};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_REQUESTS_PER_WINDOW: u32 = 30;

/// Per-IP fixed-window limiter. `/query` fans a single request out to
/// multiple upstream search engines, so letting it be hit unbounded means
/// letting *those* engines be hit unbounded through us — risking an IP ban
/// on the whole deployment, or the instance being used as a free scraping
/// proxy.
pub struct RateLimiter {
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
        }
    }
}

impl RateLimiter {
    /// Returns `true` if `ip` is still under the limit for its current
    /// window (and records the hit), `false` if it should be rejected.
    fn allow(&self, ip: IpAddr) -> bool {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();

        match hits.get_mut(&ip) {
            Some((window_start, count)) if now.duration_since(*window_start) < WINDOW => {
                if *count >= MAX_REQUESTS_PER_WINDOW {
                    false
                } else {
                    *count += 1;
                    true
                }
            }
            _ => {
                hits.insert(ip, (now, 1));
                true
            }
        }
    }
}

/// Request guard that enforces [`RateLimiter`] on whichever route declares
/// it as a parameter. Rejects with `429 Too Many Requests` once an IP
/// exceeds [`MAX_REQUESTS_PER_WINDOW`] in a rolling [`WINDOW`].
pub struct RateLimited;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for RateLimited {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let limiter = req
            .rocket()
            .state::<RateLimiter>()
            .expect("RateLimiter must be managed state");

        // Falls back to a fixed key if we genuinely can't determine the
        // caller's address, so a misconfigured proxy fails closed (shared
        // rate limit) rather than open (no limit at all).
        let ip = req
            .client_ip()
            .unwrap_or_else(|| IpAddr::from([0, 0, 0, 0]));

        if limiter.allow(ip) {
            Outcome::Success(RateLimited)
        } else {
            Outcome::Error((Status::TooManyRequests, ()))
        }
    }
}
