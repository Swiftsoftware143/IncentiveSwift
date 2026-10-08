//! Default-deny routing (kanban t_28a832dd).
//!
//! # The rule
//!
//! **A mounted route is PRIVATE unless it appears in [`PUBLIC_ROUTES`].** The router is flat — one
//! `Router::new()` with ~224 `.route(...)` calls and per-handler `AuthenticatedUser` extractors —
//! so before this module the only thing standing between a newly mounted route and the public
//! internet was the author remembering to add an extractor. `security::auth::admin_guard` covers
//! the operator surface (`/api/v1/admin/*` + `OPERATOR_ONLY_PATHS`), and NOTHING covered the rest:
//! a `get(handler)` whose handler forgot the extractor answered an anonymous caller 200.
//!
//! This middleware closes that class at the boundary: any request to a matched route whose path
//! does not match a committed public template must present a credential.
//!
//! # What counts as a credential (the three arms)
//!
//! 1. `Authorization: Bearer <api key>` — an IncentiveSwift-issued key (`is_key_…`, bcrypt in
//!    `api_keys`), the credential FunnelSwift-Mobile presents to `GET /api/v1/campaigns`.
//! 2. `Authorization: Bearer <jwt>` — an app session (HS256 over the app's `JWT_SECRET`).
//! 3. `X-Internal-Sync-Key: <INTERNAL_SYNC_KEY>` (a sibling service / cron), and its sibling
//!    spelling `X-Internal-Key` — the same shared secret `security::auth::admin_guard` and
//!    `register_business`'s own auth already accept, so an internal caller that works today keeps
//!    working.
//!
//! The middleware asks only "is there a credential?"; the handler still owns *authorisation*
//! (which account, which role). So this can never widen a caller's reach — it can only refuse an
//! anonymous one.
//!
//! # Where the allowlist came from (measured 2026-10-05)
//!
//! Every one of the 224 mounted routes was classified from source: 151 take `AuthenticatedUser`
//! (already private), 15 more sit behind `admin_guard`, and 58 answered an anonymous caller. Of
//! those 58, 49 are deliberately-public (participant/player surfaces, auth, catalogues, webhooks
//! whose own signature or shared key is the credential) and are listed below. The remaining four
//! were anonymous by accident and become private here — the intended behaviour change:
//!
//! ```text
//! GET  /api/v1/available-providers                  (read-only catalogue; only the console calls it)
//! GET  /api/v1/loyalty/plans                        (read-only catalogue; only the console calls it)
//! POST /api/v1/loyalty/supplier/milestone           <- AN ANONYMOUS WRITER, 0 callers anywhere
//! GET  /api/v1/loyalty/supplier/milestones/:id      <- an anonymous read of a business's milestones
//! ```
//!
//! # Adding a route
//!
//! Leave it out of [`PUBLIC_ROUTES`] and it is private. Do NOT add an entry unless the route must
//! answer a caller that presents no credential — and then add a unit-test leg for it in
//! `route_policy`'s test module so the decision is recorded with its reason.

use crate::error::AppError;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

/// Routes that may be reached with NO credential at all.
///
/// Templates use axum's `:param` spelling and match by segment (see [`is_public_route`]), so
/// `/api/v1/play/:id` accepts `/api/v1/play/anything` but not `/api/v1/play/a/b`.
pub const PUBLIC_ROUTES: &[&str] = &[
    // --- liveness ---------------------------------------------------------------
    "/api/v1/health",
    // --- account entry points ---------------------------------------------------
    "/api/v1/auth/register",
    "/api/v1/auth/login",
    "/api/v1/auth/forgot-password",
    "/api/v1/auth/reset-password",
    "/api/v1/business/register",
    // --- public catalogues ------------------------------------------------------
    "/api/v1/plans",
    // A sibling service verifies a pasted API key here by contract (the app's own
    // `POST /api/v1/api-keys/verify` answers `{"valid": …}` for any well-formed request).
    "/api/v1/api-keys/verify",
    // --- account email branding (kanban t_feab8aff) ------------------------------
    // The LOGO read, and only the read. A mail client renders `<img src>` with no credential of any
    // kind, so a token-gated logo simply never appears in the recipient's inbox. It returns the
    // bytes one account uploaded, keyed by an unguessable uuid, with the content type sniffed from
    // those bytes at upload time. Its two WRITE twins
    // (`POST|DELETE /api/v1/settings/branding/logo`) are deliberately NOT here: writing a logo is an
    // authenticated act on the caller's own account.
    "/api/v1/branding/logo/:tenant_id",
    // --- the account's own profile picture (kanban t_4fcbe895) -------------------
    // The READ, and only the read. Same reasoning as the branding logo above: an HTML `<img src>`
    // carries no credential, so a token-gated picture would simply never render in the console. It
    // returns the bytes one account uploaded, keyed by an unguessable uuid, with the content type
    // sniffed from those bytes at upload time; an account with no picture answers 404. Its WRITE
    // twin (`POST /api/v1/auth/avatar`) is deliberately NOT here: uploading a picture is an
    // authenticated act on the caller's own account.
    "/api/v1/auth/avatar/:user_id",
    // --- participant / player surfaces (the campaign's own audience) -------------
    "/api/v1/play/:id",
    "/api/v1/play/:id/dashboard",
    "/api/v1/play/:campaign_id/questions",
    "/api/v1/embed/campaign/all",
    "/api/v1/embed/campaign/:slug",
    "/api/v1/embed/:id",
    "/api/v1/widget/:hash",
    "/api/v1/widget/:hash/config",
    "/api/v1/quiz/:campaign_id/submit",
    "/api/v1/iqs/play/:slug",
    "/api/v1/iqs/play/:slug/submit",
    "/api/v1/campaigns/subdomain/:t_slug",
    "/api/v1/campaigns/:slug/widget",
    "/api/v1/campaigns/:slug/spin",
    "/api/v1/campaigns/:slug/spin-status",
    "/api/v1/campaigns/:slug/score-reveal",
    "/api/v1/campaigns/:slug/scratch-card",
    "/api/v1/campaigns/:slug/mystery",
    "/api/v1/campaigns/:slug/countdown",
    "/api/v1/campaigns/:slug/poll",
    "/api/v1/campaigns/:slug/poll/results",
    "/api/v1/campaigns/:slug/chat",
    "/api/v1/campaigns/:slug/long-form-qualifier",
    "/api/v1/campaigns/:slug/earn/verify",
    "/api/v1/campaigns/:slug/leaderboard",
    // Deliberately public by design: the player redeems a promo code with no session.
    "/api/v1/campaigns/:campaign_id/redeem-code",
    "/api/v1/entries",
    "/api/v1/raffles/:slug/enter",
    // Viral share links: the participant clicks through from a social post.
    "/api/v1/earn/:channel_code",
    "/api/v1/c/:campaign_slug",
    // --- loyalty participant surfaces -------------------------------------------
    "/api/v1/loyalty/checkin",
    "/api/v1/loyalty/online/visit",
    "/api/v1/loyalty/online/share",
    "/api/v1/loyalty/online/referral-click",
    "/api/v1/loyalty/online/stats/:code",
    "/api/v1/loyalty/public/program/:slug",
    "/api/v1/loyalty/secret-code/verify",
    // --- inbound webhooks (their own signature / shared key is the credential) --
    "/api/v1/loyalty/webhook/stripe",
    "/api/v1/channels/inbound",
    // Read by the businesses themselves (the served admin guide documents it).
    "/api/v1/treasury/rules",
    // Authenticates itself with the business's OWN credential scheme (`X-Internal-Key`, or an
    // argon2 API key) inside `business_handler::verify_business_auth` — a scheme this middleware
    // cannot and must not duplicate, so the handler keeps ownership of it.
    "/api/v1/business/:business_id/stats",
];

/// Is `path` (a concrete request path) one of the committed public templates?
///
/// Segment-wise match: the split lengths must agree and every template segment is either a
/// `:param` (any one non-empty segment) or the identical literal. That is deliberately stricter
/// than a string prefix — `/api/v1/plansX` is a different route and must not be caught, and a
/// template can never accidentally swallow a longer path.
pub fn is_public_route(path: &str) -> bool {
    PUBLIC_ROUTES.iter().any(|t| matches_template(t, path))
}

/// Does one template match one concrete path?
fn matches_template(template: &str, path: &str) -> bool {
    let t: Vec<&str> = template.split('/').collect();
    let p: Vec<&str> = path.split('/').collect();
    if t.len() != p.len() {
        return false;
    }
    t.iter().zip(p.iter()).all(|(tseg, pseg)| {
        if let Some(_param) = tseg.strip_prefix(':') {
            !pseg.is_empty()
        } else {
            tseg == pseg
        }
    })
}

/// Default-deny at the request boundary.
///
/// Mounted with `Router::route_layer` (kanban t_a0c272ec), so it only ever sees requests that
/// MATCHED a route — an unmatched path still gets the router's own 404 and never a misleading 401.
///
/// That distinction is MEASURED, not assumed. Under the original `Router::layer` mount (kanban
/// t_28a832dd) every unmatched path on the live origin (127.0.0.1:8083, probed 2026-10-06) answered
/// `401 {"code":401,"error":"Authentication required"}` — `/nonexistent.js`, `/admin`, `/foobar`,
/// `/api/v1/nope`, `/api/v1/contacts/xyz` — because `Router::layer` also wraps the fallback. Only
/// `route_layer` layers `path_router` alone (`routing/mod.rs` passes `fallback_router` through
/// untouched), which is what axum recommends for an authorization gate: it would otherwise
/// "convert a `404 Not Found` into a `401 Unauthorized`". A matched private route reached without a
/// credential still answers 401.
///
/// **Why the credential is read OUT of the request before the await.** axum's `Body` is
/// deliberately `!Sync`, so `&Request<Body>` is not `Send`; a future that held such a reference
/// across an await would make this middleware's future `!Send` and `Router::layer` would refuse the
/// whole router (`error[E0277]: the trait bound FromFn<…>: Service<…> is not satisfied`, measured
/// while building this). The request itself is owned and `Send`, so it is fine to hold; a *borrow*
/// of it across the await is not.
pub async fn default_deny(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    use axum::response::IntoResponse;

    // Never intercept a CORS preflight: it carries no credential by construction and the
    // `CorsLayer` above answers it.
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }

    if is_public_route(req.uri().path()) {
        return next.run(req).await;
    }

    let internal_ok = presents_internal_key(&state, &req);
    let bearer = bearer_token(&req).map(str::to_owned);

    if internal_ok {
        return next.run(req).await;
    }
    if let Some(token) = bearer {
        match credential_is_valid(&state, &token).await {
            Ok(true) => return next.run(req).await,
            Ok(false) => {}
            Err(e) => return e.into_response(),
        }
    }

    tracing::debug!(
        path = %req.uri().path(),
        "default-deny: private route reached with no credential"
    );
    AppError::Unauthorized("Authentication required".to_string()).into_response()
}

/// Arm 3 of the credential rule: the shared internal key, in either spelling the app already
/// accepts for it (`admin_guard` uses the first, `business_handler::verify_business_auth` the
/// second).
fn presents_internal_key(state: &crate::state::AppState, req: &Request) -> bool {
    let configured = state.config.internal_sync_key.as_str();
    if configured.is_empty() {
        return false;
    }
    ["X-Internal-Sync-Key", "X-Internal-Key"]
        .iter()
        .any(|h| req.headers().get(*h).and_then(|v| v.to_str().ok()) == Some(configured))
}

/// The `Bearer` value of the `Authorization` header, if it is one.
fn bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Arms 1 and 2: an issued API key (bcrypt against `api_keys` / the legacy store) or an app JWT.
///
/// Takes only `&AppState` and `&str` — no reference to the request — so it is `Send` and can be
/// awaited from [`default_deny`]. `Err` means the credential store itself failed (a database
/// error), which is passed through as the store's own 5xx rather than read as "no credential".
async fn credential_is_valid(
    state: &crate::state::AppState,
    token: &str,
) -> Result<bool, AppError> {
    match crate::security::auth::validate_api_key(state, token).await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(crate::security::jwt::verify_jwt(token, &state.config.jwt_secret).is_ok()),
        Err(AppError::Unauthorized(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_public_route, matches_template, PUBLIC_ROUTES};

    /// The route templates `src/main.rs` mounts, read from the source at compile time. A route
    /// joining or leaving the router changes this set, which is what makes the parity test below
    /// fail loudly instead of the allowlist silently rotting.
    fn mounted_routes() -> Vec<String> {
        const MAIN: &str = include_str!("../main.rs");
        let bytes = MAIN.as_bytes();
        let needle = b".route(";
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + needle.len() <= bytes.len() {
            if &bytes[i..i + needle.len()] == needle {
                let mut j = i + needle.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'"' {
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k] != b'"' {
                        k += 1;
                    }
                    out.push(MAIN[j + 1..k].to_string());
                }
                i = j;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Every committed public template must name a route the router actually mounts. A typo, or an
    /// entry left behind after a route is renamed, is a live hole the moment some other route takes
    /// that path — so it fails here instead.
    #[test]
    fn every_public_entry_names_a_mounted_route() {
        let mounted = mounted_routes();
        assert!(
            mounted.len() > 200,
            "route census found only {} routes — the extractor is broken, not the allowlist",
            mounted.len()
        );
        for entry in PUBLIC_ROUTES {
            assert!(
                mounted.iter().any(|m| m == entry),
                "PUBLIC_ROUTES entry {entry:?} is not a mounted route \
                 (the router's own spelling must match exactly)"
            );
        }
    }

    /// The allowlist has no duplicates — a copy-paste mistake here is invisible in production but
    /// makes the census above meaningless.
    #[test]
    fn public_routes_are_unique() {
        let mut sorted: Vec<&str> = PUBLIC_ROUTES.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "duplicate entry in PUBLIC_ROUTES");
    }

    /// Tenant data surfaces and the operator prefix must stay private. These are the routes whose
    /// exposure to an anonymous caller is the whole reason this module exists, so they get an
    /// explicit negative control rather than relying on the absence of an allowlist entry.
    #[test]
    fn tenant_and_operator_surfaces_are_not_public() {
        for p in [
            "/api/v1/campaigns",
            "/api/v1/campaigns/some-slug",
            "/api/v1/campaigns/some-slug/integrations",
            "/api/v1/contacts",
            "/api/v1/settings",
            "/api/v1/settings/email/test",
            "/api/v1/dashboard/stats",
            "/api/v1/analytics/overview",
            "/api/v1/tags",
            "/api/v1/email-templates",
            "/api/v1/email-templates/merge-fields",
            "/api/v1/portfolio-companies",
            "/api/v1/provider-keys",
            "/api/v1/available-providers",
            "/api/v1/loyalty/plans",
            "/api/v1/loyalty/supplier/milestone",
            "/api/v1/loyalty/supplier/milestones/0a1b2c3d",
            "/api/v1/credits/balance",
            "/api/v1/auth/me",
            // The WRITE twin of the public avatar read (kanban t_4fcbe895): uploading a picture is
            // an authenticated act on the caller's own account, so it must stay private.
            "/api/v1/auth/avatar",
            "/api/v1/admin/tenants",
            "/api/v1/admin/tenants/bulk-delete",
            "/api/v1/admin/plans",
            "/api/v1/admin/treasury/summary",
            "/api/v1/admin/credits",
            "/api/v1/internal/portfolio-sync",
            "/api/v1/internal/tag-provision",
            "/api/v1/checkout/create",
        ] {
            assert!(!is_public_route(p), "{p} must be private by default");
        }
    }

    /// The other side: the surfaces the participants and the app's own signup flow need must stay
    /// reachable with no credential. A blanket gate is the failure mode this list exists to stop.
    #[test]
    fn participant_and_entry_surfaces_stay_public() {
        for p in [
            "/api/v1/health",
            "/api/v1/auth/register",
            "/api/v1/auth/login",
            "/api/v1/auth/forgot-password",
            "/api/v1/auth/reset-password",
            "/api/v1/business/register",
            "/api/v1/plans",
            "/api/v1/api-keys/verify",
            // The account's own profile picture (kanban t_4fcbe895): a bare `<img src>` in the
            // console cannot carry a bearer token, so this read must answer anonymously.
            "/api/v1/auth/avatar/0a1b2c3d",
            "/api/v1/entries",
            "/api/v1/play/0a1b2c3d",
            "/api/v1/play/0a1b2c3d/dashboard",
            "/api/v1/play/0a1b2c3d/questions",
            "/api/v1/embed/campaign/all",
            "/api/v1/embed/campaign/some-slug",
            "/api/v1/embed/0a1b2c3d",
            "/api/v1/widget/abc123",
            "/api/v1/widget/abc123/config",
            "/api/v1/campaigns/subdomain/some-slug",
            "/api/v1/campaigns/some-slug/spin",
            "/api/v1/campaigns/some-slug/score-reveal",
            "/api/v1/campaigns/some-slug/poll/results",
            "/api/v1/campaigns/some-slug/earn/verify",
            "/api/v1/campaigns/some-slug/leaderboard",
            "/api/v1/campaigns/0a1b2c3d/redeem-code",
            "/api/v1/raffles/some-slug/enter",
            "/api/v1/earn/SOMECODE",
            "/api/v1/c/some-slug",
            "/api/v1/loyalty/checkin",
            "/api/v1/loyalty/online/visit",
            "/api/v1/loyalty/public/program/some-slug",
            "/api/v1/loyalty/webhook/stripe",
            "/api/v1/channels/inbound",
            "/api/v1/treasury/rules",
            "/api/v1/business/0a1b2c3d/stats",
        ] {
            assert!(is_public_route(p), "{p} must stay reachable anonymously");
        }
    }

    /// Segment matching, not string prefixing: a template must not swallow a longer or differently
    /// shaped path, and a `:param` must not match an empty segment.
    #[test]
    fn matching_is_segment_exact() {
        assert!(matches_template("/api/v1/plans", "/api/v1/plans"));
        assert!(!matches_template("/api/v1/plans", "/api/v1/plansX"));
        assert!(!matches_template("/api/v1/plans", "/api/v1/plans/extra"));
        assert!(!matches_template("/api/v1/plans", "/api/v1/plays"));
        assert!(matches_template("/api/v1/play/:id", "/api/v1/play/x"));
        assert!(!matches_template("/api/v1/play/:id", "/api/v1/play"));
        assert!(!matches_template("/api/v1/play/:id", "/api/v1/play/x/y"));
        assert!(!matches_template("/api/v1/play/:id", "/api/v1/play/"));
    }
}
