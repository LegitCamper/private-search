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

/// Hard cap on distinct IPs tracked at once. Bounds worst-case memory even
/// if a flood of distinct addresses arrives faster than sweeps can run.
const MAX_TRACKED_IPS: usize = 100_000;

/// Per-IP fixed-window limiter. `/query` fans a single request out to
/// multiple upstream search engines, so letting it be hit unbounded means
/// letting *those* engines be hit unbounded through us — risking an IP ban
/// on the whole deployment, or the instance being used as a free scraping
/// proxy.
pub struct RateLimiter {
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    window: Duration,
    max_per_window: u32,
    // Guards the sweep cadence rather than the map itself, so it is a
    // separate lock: checking "is it time to sweep" must not require
    // holding `hits`'s lock for longer than the sweep itself takes.
    last_sweep: Mutex<Instant>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(WINDOW, MAX_REQUESTS_PER_WINDOW)
    }
}

impl RateLimiter {
    /// Exposed (rather than hardcoding `WINDOW`/`MAX_REQUESTS_PER_WINDOW`
    /// everywhere) so tests can use a short window instead of waiting on the
    /// real 60s production one.
    fn new(window: Duration, max_per_window: u32) -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
            window,
            max_per_window,
            last_sweep: Mutex::new(Instant::now()),
        }
    }

    #[cfg(test)]
    fn tracked_ips(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    /// Drops every tracked entry whose window has expired. Those entries can
    /// only ever hit the `_ =>` arm in `allow` from here on, so removing them
    /// changes no behavior — this is purely about reclaiming memory for IPs
    /// that never come back.
    ///
    /// Runs opportunistically from inside `allow`, at most once per
    /// `window`, rather than on a background task: the map is cheap to walk,
    /// and a background task would mean another handle to spawn, own, and
    /// shut down for what is otherwise a stateless, self-contained type.
    fn sweep(&self, now: Instant) {
        let mut last_sweep = self.last_sweep.lock().unwrap();
        if now.duration_since(*last_sweep) < self.window {
            return;
        }
        *last_sweep = now;
        drop(last_sweep);

        let mut hits = self.hits.lock().unwrap();
        hits.retain(|_, (window_start, _)| now.duration_since(*window_start) < self.window);

        // A sweep alone isn't enough if distinct IPs are arriving faster
        // than we can reclaim them — an active flood, not stale leftovers.
        // A per-IP limiter can't meaningfully constrain an attacker who is
        // spraying that many distinct addresses anyway, so a bounded map
        // that occasionally forgets everyone is strictly better than an
        // unbounded one that OOMs the process.
        if hits.len() > MAX_TRACKED_IPS {
            hits.clear();
        }
    }

    /// Returns `true` if `ip` is still under the limit for its current
    /// window (and records the hit), `false` if it should be rejected.
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        self.sweep(now);

        let mut hits = self.hits.lock().unwrap();

        match hits.get_mut(&ip) {
            Some((window_start, count)) if now.duration_since(*window_start) < self.window => {
                if *count >= self.max_per_window {
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

#[cfg(test)]
mod test {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[test]
    fn allows_requests_under_the_limit() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);
        assert!(limiter.allow(ip(1)));
        assert!(limiter.allow(ip(1)));
        assert!(limiter.allow(ip(1)));
    }

    #[test]
    fn rejects_once_the_limit_is_exceeded_within_a_window() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);
        for _ in 0..3 {
            assert!(limiter.allow(ip(1)));
        }
        assert!(
            !limiter.allow(ip(1)),
            "4th request in the window should be rejected"
        );
    }

    #[test]
    fn tracks_each_ip_independently() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.allow(ip(1)));
        assert!(!limiter.allow(ip(1)));
        // A different IP has its own budget, unaffected by ip(1)'s.
        assert!(limiter.allow(ip(2)));
    }

    #[test]
    fn resets_once_the_window_elapses() {
        let limiter = RateLimiter::new(Duration::from_millis(20), 1);
        assert!(limiter.allow(ip(1)));
        assert!(!limiter.allow(ip(1)), "still inside the first window");

        std::thread::sleep(Duration::from_millis(40));

        assert!(limiter.allow(ip(1)), "a new window should reset the budget");
    }

    #[test]
    fn stale_entries_are_evicted_so_the_map_does_not_grow_without_bound() {
        let limiter = RateLimiter::new(Duration::from_millis(20), 5);

        for i in 0u32..200 {
            let [a, b, c, d] = i.to_be_bytes();
            assert!(limiter.allow(IpAddr::from([a, b, c, d])));
        }

        std::thread::sleep(Duration::from_millis(40));

        // One more request, from an IP not already tracked, to trigger the
        // opportunistic sweep.
        assert!(limiter.allow(IpAddr::from([255, 255, 255, 255])));

        assert!(
            limiter.tracked_ips() <= 2,
            "expected stale entries to be evicted, found {} tracked IPs",
            limiter.tracked_ips()
        );
    }

    #[test]
    fn a_sweep_does_not_reset_an_active_ips_budget() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);

        // Populate the map with other IPs so a sweep has something to walk,
        // though none of these are stale within this test's long window.
        for n in 2u8..10 {
            assert!(limiter.allow(ip(n)));
        }

        for _ in 0..3 {
            assert!(limiter.allow(ip(1)));
        }
        assert!(
            !limiter.allow(ip(1)),
            "budget should still be exhausted after a sweep runs"
        );
    }
}
