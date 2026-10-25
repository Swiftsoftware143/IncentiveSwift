//! Failed-sign-in throttle for the password route (cron 2026-10-25).
//!
//! MEASURED DEFECTS this closes, on this app's LIVE build before this module existed:
//!
//!  1. twenty-two consecutive wrong-password attempts on `POST /api/v1/auth/login` were answered
//!     `401 Invalid email or password` — 22/22, no `Retry-After` — so a guesser could work through
//!     a password list at wire speed, and every guess costs a full Argon2 verify.
//!  2. an address with NO account answered in **1.7 ms** while a real address with a wrong
//!     password took **23.0 ms** (medians of 5): a 13x gap, because the absent arm returned before
//!     hashing. That named which addresses are customers, and it is closed in the same change by
//!     always running one verify against the dummy hash in `auth_handler::dummy_password_hash`.
//!
//! ## Why the app must count when nginx already limits the rate
//!
//! nginx fronts this app with `limit_req zone=api_visitor burst=40 nodelay` — 20 requests/second
//! per client, which is a FLOOD control, not a guessing control: a guesser pacing 5/second stays
//! under it for ever, and 22 attempts in a burst were served without complaint (measured). This
//! module counts FAILURES, so the 30th wrong password inside a quarter hour pauses the address
//! whatever rate it arrived at.
//!
//! ## Why the key is the ADDRESS and not the account
//!
//! A per-account lockout hands an attacker a denial of service against any victim whose address
//! they know — a few junk attempts and the customer cannot sign in. That is the fleet's own
//! decision, made when WorkflowSwift's limiter was written, and it is why this counter never
//! learns an address. The mirror image: everybody behind one NAT/office shares a bucket, so the
//! threshold is sized for a shared egress. Only failures are counted and a successful sign-in
//! clears the history, so a customer who mistyped twice is never affected. Nothing is persisted —
//! a restart forgets every counter, which is right for a burst limiter and honest about not being
//! an audit log.
//!
//! ## Trusting the address
//!
//! `X-Real-IP` is read FIRST because `nginx.conf`'s `real_ip` block rewrites `$remote_addr` from
//! Cloudflare's `CF-Connecting-IP` (only when the TCP peer is inside CF's ranges) and every server
//! block then sends `X-Real-IP $remote_addr` — so the header carries the VISITOR, not the edge.
//! nginx replaces a header it sets, so a client cannot choose it. `CF-Connecting-IP` is the second
//! arm and the rightmost `X-Forwarded-For` hop the third; a caller presenting none shares one
//! bucket, so omitting the header is not an escape.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failed sign-ins allowed from one address inside [`WINDOW`].
pub const MAX_FAILURES: u32 = 30;
/// The rolling window the failures are counted in.
pub const WINDOW: Duration = Duration::from_secs(15 * 60);
/// How long an address is paused once it has spent its failures.
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Bound on the bucket table. Every bucket is keyed by a client-supplied string, so an unbounded
/// map would be a memory-exhaustion vector of its own; past this size new callers share one
/// bucket instead of growing the map (the same shape WorkflowSwift's limiter uses).
const MAX_ENTRIES: usize = 20_000;
/// Bucket shared by every address once the table is at capacity.
const OVERFLOW_BUCKET: &str = "__overflow__";

#[derive(Debug, Default, Clone, Copy)]
struct Entry {
    failures: u32,
    window_start: Option<Instant>,
    blocked_until: Option<Instant>,
}

impl Entry {
    /// Whether this bucket has anything left to say at `now` (used to prune dead buckets).
    fn live(&self, now: Instant) -> bool {
        match (self.window_start, self.blocked_until) {
            (_, Some(until)) => until > now,
            (Some(start), None) => now.duration_since(start) < WINDOW,
            (None, None) => false,
        }
    }

    /// Seconds this bucket still has to wait, or `None` when the attempt may proceed.
    fn wait_for(&self, now: Instant) -> Option<u64> {
        match self.blocked_until {
            Some(until) if until > now => Some((until - now).as_secs().max(1)),
            _ => None,
        }
    }
}

/// The counter itself. `Clone` is cheap (the table is behind one `Arc`), so it lives in
/// `AppState` like every other shared handle.
#[derive(Clone)]
pub struct LoginThrottle {
    entries: std::sync::Arc<Mutex<HashMap<String, Entry>>>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Take the table lock, recovering from poisoning instead of panicking: this runs on every
    /// sign-in, so a panic elsewhere in the process must not become a permanent 500.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `Ok(())` when this attempt may proceed; `Err(seconds)` when the address must wait.
    ///
    /// Read-only apart from dropping an expired bucket, so a blocked caller cannot extend its
    /// own block by hammering the route.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        let now = Instant::now();
        let mut entries = self.lock();
        if let Some(entry) = entries.get(key) {
            if let Some(wait) = entry.wait_for(now) {
                return Err(wait);
            }
            if !entry.live(now) {
                entries.remove(key);
            }
        }
        Ok(())
    }

    /// Count one failed attempt. Reaching [`MAX_FAILURES`] inside [`WINDOW`] pauses the address.
    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let mut entries = self.lock();
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(key) {
            entries.retain(|_, e| e.live(now));
            if entries.len() >= MAX_ENTRIES {
                // Still full of live buckets: share one instead of growing the map.
                let entry = entries.entry(OVERFLOW_BUCKET.to_string()).or_default();
                bump(entry, now);
                return;
            }
        }
        let entry = entries.entry(key.to_string()).or_default();
        bump(entry, now);
    }

    /// A successful sign-in clears the address's history.
    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }
}

/// Add one failure to a bucket, starting a fresh window when the old one has elapsed and pausing
/// the address once the allowance is spent.
fn bump(entry: &mut Entry, now: Instant) {
    let expired = match entry.window_start {
        Some(start) => now.duration_since(start) >= WINDOW,
        None => true,
    };
    if expired {
        entry.failures = 1;
        entry.window_start = Some(now);
        entry.blocked_until = None;
        return;
    }
    entry.failures = entry.failures.saturating_add(1);
    if entry.failures >= MAX_FAILURES {
        entry.blocked_until = Some(now + LOCKOUT);
    }
}

/// The client address, in order of trust. See the module docs for why `X-Real-IP` is first.
pub fn client_identity(headers: &axum::http::HeaderMap) -> String {
    let read = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    if let Some(ip) = read("x-real-ip") {
        return ip;
    }
    if let Some(ip) = read("cf-connecting-ip") {
        return ip;
    }
    if let Some(xff) = read("x-forwarded-for") {
        if let Some(last) = xff
            .rsplit(',')
            .map(|hop| hop.trim())
            .find(|hop| !hop.is_empty())
        {
            return last.to_string();
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(t: &LoginThrottle, key: &str) {
        for _ in 0..MAX_FAILURES {
            assert!(t.check(key).is_ok());
            t.record_failure(key);
        }
    }

    #[test]
    fn an_address_is_paused_only_after_spending_its_allowance() {
        let t = LoginThrottle::new();
        for _ in 0..(MAX_FAILURES - 1) {
            assert!(t.check("1.2.3.4").is_ok());
            t.record_failure("1.2.3.4");
        }
        // One short of the allowance: still allowed.
        assert!(t.check("1.2.3.4").is_ok());
        t.record_failure("1.2.3.4");
        // Spent: the next attempt must wait, and the wait is reported in seconds.
        let wait = t.check("1.2.3.4").expect_err("spent allowance must pause");
        assert!(wait > 0 && wait <= LOCKOUT.as_secs(), "wait={wait}");
        // Another address is untouched — the pause is per address, not global.
        assert!(t.check("5.6.7.8").is_ok());
    }

    #[test]
    fn a_success_clears_the_history() {
        let t = LoginThrottle::new();
        spend(&t, "1.2.3.4");
        assert!(t.check("1.2.3.4").is_err());
        t.clear("1.2.3.4");
        assert!(t.check("1.2.3.4").is_ok());
    }

    #[test]
    fn hammering_a_paused_address_does_not_extend_its_pause() {
        let t = LoginThrottle::new();
        spend(&t, "1.2.3.4");
        let first = t.check("1.2.3.4").expect_err("paused");
        for _ in 0..50 {
            let _ = t.check("1.2.3.4");
        }
        let after = t.check("1.2.3.4").expect_err("still paused");
        // The remaining wait only ever shrinks; it is a deadline, not a rolling one.
        assert!(after <= first, "first={first} after={after}");
    }

    #[test]
    fn the_identity_prefers_the_header_nginx_sets() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(client_identity(&h), "unknown");
        h.insert("x-forwarded-for", "9.9.9.9, 8.8.8.8".parse().unwrap());
        assert_eq!(client_identity(&h), "8.8.8.8");
        h.insert("cf-connecting-ip", "7.7.7.7".parse().unwrap());
        assert_eq!(client_identity(&h), "7.7.7.7");
        h.insert("x-real-ip", "6.6.6.6".parse().unwrap());
        assert_eq!(client_identity(&h), "6.6.6.6");
    }
}
