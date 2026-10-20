//! Opt-in, in-memory request rate limiting per caller.
//!
//! Each caller has one bucket, enforced with the generic cell rate algorithm
//! (GCRA, equivalent to a token bucket): a caller may send `burst` requests at
//! once and then one request every `60 s / requests_per_minute`. A bucket is a
//! single theoretical arrival time, so an idle caller holds no state and is
//! pruned when the key map is full, at most once per sweep interval. The limit
//! is per process.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use tokio::time::Instant;
use tracing::warn;

use crate::auth::AuthenticatedPrincipal;
use crate::error_response::{ErrorFormat, GatewayError};

/// Most distinct callers tracked at once before new ones share one bucket.
const MAX_KEYS: usize = 100_000;
/// Minimum spacing between warnings that new callers share the overflow bucket.
const OVERFLOW_WARNING_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_AFTER_MS: HeaderName = HeaderName::from_static("retry-after-ms");
const LIMIT_REQUESTS: HeaderName = HeaderName::from_static("x-ratelimit-limit-requests");
const REMAINING_REQUESTS: HeaderName = HeaderName::from_static("x-ratelimit-remaining-requests");
const RESET_REQUESTS: HeaderName = HeaderName::from_static("x-ratelimit-reset-requests");
/// Minimum spacing between sweeps of a full key map for idle callers.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// A validated per-caller request rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    requests_per_minute: NonZeroU32,
    burst: NonZeroU32,
}

/// Why a rate-limit configuration is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RateLimitConfigError {
    #[error("rate limit burst {burst} exceeds one hour of requests ({max})")]
    BurstTooLarge { burst: u32, max: u64 },
}

impl RateLimitConfig {
    /// A rate of `requests_per_minute`, allowing `burst` requests at once
    /// (`requests_per_minute` when unset).
    ///
    /// # Errors
    ///
    /// Returns [`RateLimitConfigError::BurstTooLarge`] when `burst` exceeds
    /// one hour of requests at that rate.
    pub fn new(requests_per_minute: NonZeroU32, burst: Option<NonZeroU32>) -> Result<Self, RateLimitConfigError> {
        let burst = burst.unwrap_or(requests_per_minute);
        let max = u64::from(requests_per_minute.get()) * 60;
        if u64::from(burst.get()) > max {
            return Err(RateLimitConfigError::BurstTooLarge {
                burst: burst.get(),
                max,
            });
        }
        Ok(Self {
            requests_per_minute,
            burst,
        })
    }

    #[must_use]
    pub const fn requests_per_minute(self) -> NonZeroU32 {
        self.requests_per_minute
    }

    #[must_use]
    pub const fn burst(self) -> NonZeroU32 {
        self.burst
    }

    /// The steady-state spacing between requests.
    fn interval(self) -> Duration {
        Duration::from_secs(60) / self.requests_per_minute.get()
    }
}

/// Whose bucket a request is charged to.
///
/// Never a credential the gateway does not verify, such as a bearer token or
/// `x-api-key` without OIDC: a client could send a fresh one with every
/// request, and on the executor route the gateway may call the upstream with
/// its own key, so nothing would ever check it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RateLimitKey {
    /// The verified OIDC token's issuer and subject.
    Principal { issuer: String, subject: String },
    /// The connection's peer: an IPv4 address, or an IPv6 /64 network.
    Peer(IpAddr),
    /// A request whose peer address is unknown, because the router was served
    /// without connection information.
    Anonymous,
    /// Shared by new callers while every tracked caller is active.
    Overflow,
}

impl RateLimitKey {
    /// The bucket for a request: its verified principal, else its peer address.
    pub(crate) fn for_caller(principal: Option<&AuthenticatedPrincipal>, peer: Option<SocketAddr>) -> Self {
        if let Some(principal) = principal {
            return Self::Principal {
                issuer: principal.issuer().to_owned(),
                subject: principal.subject().to_owned(),
            };
        }
        peer.map_or(Self::Anonymous, |peer| Self::Peer(caller_network(peer.ip())))
    }
}

/// An IPv4 address as it is; an IPv6 address as its /64 network, since one
/// host commonly holds a whole /64. An IPv4-mapped IPv6 address is its IPv4 caller.
fn caller_network(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & (u128::MAX << 64))),
    }
}

/// The outcome of charging one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allowed,
    /// The next request is allowed after `retry_after`; the bucket is full
    /// again after `reset`.
    Limited {
        retry_after: Duration,
        reset: Duration,
    },
}

/// Per-caller request buckets for one process.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    config: RateLimitConfig,
    interval: Duration,
    tolerance: Duration,
    max_keys: usize,
    state: Mutex<LimiterState>,
}

#[derive(Debug, Default)]
struct LimiterState {
    /// Each tracked caller's theoretical arrival time for its next request.
    arrivals: HashMap<RateLimitKey, Instant>,
    /// When a full map may next be swept; a sweep is due when unset.
    next_sweep: Option<Instant>,
    last_overflow_warning: Option<Instant>,
}

impl RateLimiter {
    #[must_use]
    pub(crate) fn new(config: RateLimitConfig) -> Self {
        Self::with_max_keys(config, MAX_KEYS)
    }

    fn with_max_keys(config: RateLimitConfig, max_keys: usize) -> Self {
        let interval = config.interval();
        Self {
            config,
            interval,
            tolerance: interval * (config.burst().get() - 1),
            max_keys,
            state: Mutex::default(),
        }
    }

    /// Charge one request to `key`.
    pub(crate) fn check(&self, key: RateLimitKey) -> Decision {
        let now = Instant::now();
        // The critical section has no await and cannot leave the map invalid,
        // so a poisoned lock is still usable.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = self.admit_key(&mut state, key, now);
        let arrival = state.arrivals.get(&key).map_or(now, |arrival| (*arrival).max(now));
        if arrival > now + self.tolerance {
            return Decision::Limited {
                retry_after: arrival - self.tolerance - now,
                reset: arrival - now,
            };
        }
        state.arrivals.insert(key, arrival + self.interval);
        Decision::Allowed
    }

    /// `key` itself, unless it is new and the map is full.
    ///
    /// Sweeping a full map scans every caller under the lock, so it runs at
    /// most once per [`SWEEP_INTERVAL`]; new callers in between share the
    /// overflow bucket instead of each paying for a scan.
    fn admit_key(&self, state: &mut LimiterState, key: RateLimitKey, now: Instant) -> RateLimitKey {
        if state.arrivals.len() < self.max_keys || state.arrivals.contains_key(&key) {
            return key;
        }
        if state.next_sweep.is_none_or(|due| now >= due) {
            state.next_sweep = Some(now + SWEEP_INTERVAL);
            // An idle caller's bucket is full again, so forgetting it changes nothing.
            state.arrivals.retain(|_, arrival| *arrival > now);
            if state.arrivals.len() < self.max_keys {
                return key;
            }
        }
        if state
            .last_overflow_warning
            .is_none_or(|warned| now.duration_since(warned) >= OVERFLOW_WARNING_INTERVAL)
        {
            state.last_overflow_warning = Some(now);
            warn!(
                max_keys = self.max_keys,
                "rate limiter is tracking its maximum number of callers; new callers share one bucket"
            );
        }
        RateLimitKey::Overflow
    }
}

/// A caller's bucket, carried on an admitted request so a WebSocket can charge
/// each `response.create` it accepts to the same caller.
#[derive(Clone, Debug)]
pub(crate) struct RateLimitCharge {
    limiter: Arc<RateLimiter>,
    key: RateLimitKey,
}

impl RateLimitCharge {
    /// Charge one more request to this caller.
    pub(crate) fn check(&self) -> Decision {
        self.limiter.check(self.key.clone())
    }
}

/// Charge each request to its caller's bucket before the handler runs.
///
/// Installed inside authentication, so an authenticated principal is already
/// in the request extensions.
pub(crate) async fn enforce_rate_limit(
    State(limiter): State<Arc<RateLimiter>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let key = RateLimitKey::for_caller(request.extensions().get::<AuthenticatedPrincipal>(), peer);
    match limiter.check(key.clone()) {
        Decision::Allowed => {
            request.extensions_mut().insert(RateLimitCharge { limiter, key });
            next.run(request).await
        }
        Decision::Limited { retry_after, reset } => {
            let format = ErrorFormat::for_path(request.uri().path());
            rate_limited_response(format, limiter.config, retry_after, reset)
        }
    }
}

/// The client-facing message for a limited request.
pub(crate) fn rate_limited_message(retry_after: Duration) -> String {
    format!(
        "rate limit exceeded; retry after {} seconds",
        retry_after_seconds(retry_after)
    )
}

fn rate_limited_response(
    format: ErrorFormat,
    config: RateLimitConfig,
    retry_after: Duration,
    reset: Duration,
) -> Response {
    let message = rate_limited_message(retry_after);
    let mut response = GatewayError {
        status: StatusCode::TOO_MANY_REQUESTS,
        openai_type: "rate_limit_error",
        anthropic_type: "rate_limit_error",
        code: "rate_limit_exceeded",
        message: &message,
    }
    .into_response(format);
    let headers = response.headers_mut();
    headers.insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds(retry_after)));
    headers.insert(RETRY_AFTER_MS, HeaderValue::from(retry_after_millis(retry_after)));
    headers.insert(LIMIT_REQUESTS, HeaderValue::from(config.requests_per_minute().get()));
    headers.insert(REMAINING_REQUESTS, HeaderValue::from_static("0"));
    let reset = HeaderValue::try_from(reset_duration(reset)).expect("a formatted duration is a valid header value");
    headers.insert(RESET_REQUESTS, reset);
    response
}

/// Whole seconds a client should wait, for `retry-after`; never zero.
pub(crate) fn retry_after_seconds(retry_after: Duration) -> u64 {
    ceiling(retry_after, Duration::from_secs(1))
}

/// Milliseconds a client should wait, for `retry-after-ms`; never zero.
pub(crate) fn retry_after_millis(retry_after: Duration) -> u64 {
    ceiling(retry_after, Duration::from_millis(1))
}

/// Whole `unit`s covering `wait`, rounded up from nanoseconds so the hint is
/// never shorter than the limiter requires; never zero.
fn ceiling(wait: Duration, unit: Duration) -> u64 {
    wait.as_nanos()
        .div_ceil(unit.as_nanos())
        .max(1)
        .try_into()
        .unwrap_or(u64::MAX)
}

/// The wait in the `x-ratelimit-reset-requests` style: `250ms`, `1.5s`, `2m3s`.
pub(crate) fn reset_duration(retry_after: Duration) -> String {
    let millis = retry_after_millis(retry_after);
    if millis < 1000 {
        return format!("{millis}ms");
    }
    let (minutes, millis) = (millis / 60_000, millis % 60_000);
    let seconds = format!("{}.{:03}", millis / 1000, millis % 1000);
    let seconds = seconds.trim_end_matches('0').trim_end_matches('.');
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(requests_per_minute: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig::new(NonZeroU32::new(requests_per_minute).unwrap(), NonZeroU32::new(burst)).unwrap()
    }

    fn key(name: &str) -> RateLimitKey {
        RateLimitKey::Principal {
            issuer: "https://issuer.example".to_owned(),
            subject: name.to_owned(),
        }
    }

    #[test]
    fn burst_defaults_to_the_rate_and_is_bounded_by_an_hour() {
        let rate = NonZeroU32::new(60).unwrap();
        assert_eq!(RateLimitConfig::new(rate, None).unwrap().burst(), rate);
        assert!(RateLimitConfig::new(rate, NonZeroU32::new(3600)).is_ok());
        assert_eq!(
            RateLimitConfig::new(rate, NonZeroU32::new(3601)),
            Err(RateLimitConfigError::BurstTooLarge { burst: 3601, max: 3600 })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_allowed_then_one_request_per_interval() {
        let limiter = RateLimiter::new(config(60, 3));
        for _ in 0..3 {
            assert_eq!(limiter.check(key("a")), Decision::Allowed);
        }
        assert_eq!(
            limiter.check(key("a")),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
                reset: Duration::from_secs(3)
            }
        );
        tokio::time::advance(Duration::from_millis(400)).await;
        assert_eq!(
            limiter.check(key("a")),
            Decision::Limited {
                retry_after: Duration::from_millis(600),
                reset: Duration::from_millis(2600)
            }
        );
        tokio::time::advance(Duration::from_millis(600)).await;
        assert_eq!(limiter.check(key("a")), Decision::Allowed);
        assert!(matches!(limiter.check(key("a")), Decision::Limited { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn the_steady_rate_is_always_allowed() {
        let limiter = RateLimiter::new(config(60, 1));
        for _ in 0..10 {
            assert_eq!(limiter.check(key("a")), Decision::Allowed);
            tokio::time::advance(Duration::from_secs(1)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn callers_have_independent_buckets() {
        let limiter = RateLimiter::new(config(60, 1));
        assert_eq!(limiter.check(key("a")), Decision::Allowed);
        assert!(matches!(limiter.check(key("a")), Decision::Limited { .. }));
        assert_eq!(limiter.check(key("b")), Decision::Allowed);
        assert_eq!(limiter.check(RateLimitKey::Anonymous), Decision::Allowed);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_callers_are_pruned_before_new_callers_overflow() {
        let limiter = RateLimiter::with_max_keys(config(60, 1), 2);
        assert_eq!(limiter.check(key("a")), Decision::Allowed);
        assert_eq!(limiter.check(key("b")), Decision::Allowed);
        // Both are active: a third caller is charged to the shared overflow bucket.
        assert_eq!(limiter.check(key("c")), Decision::Allowed);
        assert!(
            matches!(limiter.check(key("d")), Decision::Limited { .. }),
            "c used the overflow bucket"
        );
        // Once a and b are idle, they are forgotten and c gets its own bucket.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(limiter.check(key("c")), Decision::Allowed);
        assert!(!limiter.state.lock().unwrap().arrivals.contains_key(&key("a")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_map_is_swept_at_most_once_per_interval() {
        // 100 ms per request, so callers go idle long before the next sweep is due.
        let limiter = RateLimiter::with_max_keys(config(600, 1), 2);
        assert_eq!(limiter.check(key("a")), Decision::Allowed);
        assert_eq!(limiter.check(key("b")), Decision::Allowed);
        // Every tracked caller is active, so this sweep frees nothing.
        assert_eq!(
            limiter.check(key("new-0")),
            Decision::Allowed,
            "charged to the overflow bucket"
        );

        // a and b are idle now, but no sweep is due: a stream of new callers
        // shares the overflow bucket without scanning the map.
        tokio::time::advance(Duration::from_millis(200)).await;
        let allowed = (1..=1000)
            .filter(|index| limiter.check(key(&format!("new-{index}"))) == Decision::Allowed)
            .count();
        assert_eq!(allowed, 1, "new callers between sweeps share one bucket");
        {
            let state = limiter.state.lock().unwrap();
            assert!(state.arrivals.contains_key(&key("a")), "no sweep ran before it was due");
            assert_eq!(state.arrivals.len(), 3, "a, b, and the overflow bucket");
        }

        tokio::time::advance(SWEEP_INTERVAL).await;
        assert_eq!(limiter.check(key("late")), Decision::Allowed);
        let state = limiter.state.lock().unwrap();
        assert!(
            !state.arrivals.contains_key(&key("a")),
            "the due sweep forgot idle callers"
        );
        assert!(
            state.arrivals.contains_key(&key("late")),
            "and gave the new caller its own bucket"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_rejected_requests_at_capacity_do_not_rescan() {
        let limiter = RateLimiter::with_max_keys(config(600, 1), 2);
        assert_eq!(limiter.check(key("a")), Decision::Allowed);
        assert_eq!(limiter.check(key("b")), Decision::Allowed);
        // The untracked caller's first request sweeps once and uses the overflow bucket.
        assert_eq!(limiter.check(key("untracked")), Decision::Allowed);

        // a and b are idle now. The same untracked caller keeps sending, and
        // the overflow bucket rejects all but one of its requests.
        tokio::time::advance(Duration::from_millis(200)).await;
        let rejected = (0..1000)
            .filter(|_| matches!(limiter.check(key("untracked")), Decision::Limited { .. }))
            .count();
        assert_eq!(rejected, 999);
        let state = limiter.state.lock().unwrap();
        assert!(
            state.arrivals.contains_key(&key("a")),
            "no rejected request rescanned the full map"
        );
    }

    #[test]
    fn callers_are_keyed_by_principal_else_peer_network() {
        let from = |peer: &str| RateLimitKey::for_caller(None, Some(peer.parse().unwrap()));
        assert_eq!(from("192.0.2.7:4000"), RateLimitKey::Peer("192.0.2.7".parse().unwrap()));
        assert_eq!(
            from("192.0.2.7:4000"),
            from("192.0.2.7:5000"),
            "a new connection is the same caller"
        );
        assert_ne!(from("192.0.2.7:4000"), from("192.0.2.8:4000"));
        assert_eq!(
            from("[2001:db8:1:2::1]:4000"),
            from("[2001:db8:1:2:ffff::9]:4000"),
            "one IPv6 /64 is one caller"
        );
        assert_ne!(from("[2001:db8:1:2::1]:4000"), from("[2001:db8:1:3::1]:4000"));
        assert_eq!(from("[::ffff:192.0.2.7]:4000"), from("192.0.2.7:4000"));
        let principal = AuthenticatedPrincipal::expired_for_test();
        assert_eq!(
            RateLimitKey::for_caller(Some(&principal), Some("192.0.2.7:4000".parse().unwrap())),
            RateLimitKey::Principal {
                issuer: principal.issuer().to_owned(),
                subject: principal.subject().to_owned()
            },
            "a verified principal wins over the address"
        );
        assert_eq!(RateLimitKey::for_caller(None, None), RateLimitKey::Anonymous);
    }

    #[test]
    fn retry_hints_round_up() {
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1000)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1001)), 2);
        assert_eq!(retry_after_millis(Duration::from_micros(1500)), 2);
        assert_eq!(retry_after_seconds(Duration::ZERO), 1, "never zero");
        assert_eq!(retry_after_millis(Duration::ZERO), 1, "never zero");
        // A wait just past a boundary must not be advertised as shorter than it is.
        let nanosecond = Duration::from_nanos(1);
        assert_eq!(retry_after_seconds(Duration::from_secs(1) + nanosecond), 2);
        assert_eq!(retry_after_millis(Duration::from_millis(1) + nanosecond), 2);
        assert_eq!(retry_after_millis(Duration::from_millis(1)), 1);
        assert_eq!(reset_duration(Duration::from_secs(2) + nanosecond), "2.001s");
        assert_eq!(reset_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(reset_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(reset_duration(Duration::from_secs(2)), "2s");
        assert_eq!(reset_duration(Duration::from_millis(123_250)), "2m3.25s");
    }
}
