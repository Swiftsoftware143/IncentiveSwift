//! Authentication — API key validation (bcrypt) and JWT validation.
//!
//! SECURITY RULES:
//! - API keys: NEVER compare via direct hash equality. ALWAYS use bcrypt::verify.
//! - JWTs: Decode header+payload, verify HMAC-SHA256 signature with the JWT secret.
//!
//! REQUEST-ENTRY ACCOUNT GUARD (kanban t_a7b7b5b9): a valid-but-stale token — a signed JWT (or an
//! issued API key) whose `sub`/`user_id` names an `accounts` row that no longer exists, e.g. after
//! the account is deleted or a dump is restored without it — used to reach every writer that binds
//! `claims.sub` into a column carrying an FK to `accounts(id)` (`tenant_settings.tenant_id`,
//! `tags.account_id`, `provider_keys.account_id`, …: 33 such columns live, measured 2026-10-02). There
//! it surfaced as
//! `500 {"error":"Internal server error"}`, with `insert or update on table "…" violates foreign
//! key constraint "…"` in the log, and read exactly like a product defect. The id names WHICH
//! account is unknown, so the refusal belongs at the boundary where the request enters: this
//! extractor. Reads are untouched (a GET for a gone account is an empty list, never a 500).

use crate::error::AppError;
use axum::extract::FromRef;
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, Method},
};
use sqlx::Row;

/// Authenticated user context extracted from request.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub account_id: String,
    pub email: String,
    pub role: String,
    pub impersonating: Option<String>,
}

/// Extracts and validates Bearer token from Authorization header.
/// Supports both API keys (bcrypt-verified) and JWTs.
#[async_trait]
impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
    crate::state::AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Captured before the body is consumed: the account guard below is method-dependent.
        let method = parts.method.clone();
        let token = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                AppError::Unauthorized("Missing or invalid Authorization header".to_string())
            })?;

        let app_state = crate::state::AppState::from_ref(state);

        // Try API key validation first (bcrypt verify)
        if let Some(user) = validate_api_key(&app_state, token).await? {
            // An ISSUED API key is an integration credential, not a browser session: a password
            // change must not revoke it, so it is never measured against `sessions_valid_from`.
            guard_account_exists(&app_state.db, &method, &user.account_id, IssuedAt::Durable)
                .await?;
            return Ok(user);
        }

        // Fall back to local JWT validation
        let claims = crate::security::jwt::verify_jwt(token, &app_state.config.jwt_secret)?;

        let user = AuthenticatedUser {
            account_id: claims.sub.clone().unwrap_or_default(),
            email: claims.email.clone().unwrap_or_default(),
            role: claims
                .role
                .clone()
                .unwrap_or_else(|| "authenticated".to_string()),
            impersonating: claims.impersonating.clone(),
        };
        // `claims.iat` is what lets a password change or reset end the sessions that predate it
        // (see [`IssuedAt`] and the module doc: SESSION REVOCATION).
        guard_account_exists(
            &app_state.db,
            &method,
            &user.account_id,
            IssuedAt::Session(claims.iat),
        )
        .await?;
        Ok(user)
    }
}

/// What the request-entry account guard must do for one request.
///
/// Pure so both halves of the rule are unit-tested: a STATE-CHANGING method whose id names no
/// `accounts` row is refused, everything else is left exactly as it was (reads must keep answering
/// 200/empty for a gone account, and a known account must never pay a second query's refusal).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AccountGate {
    /// Not a state-changing method. The lookup still runs (kanban t_53fcb666) — a read naming a
    /// gone account is refused 401 — but a read naming a LIVE account pays no refusal.
    Skip,
    /// A write naming an account row that does not exist: refuse with the field-level 4xx.
    Refuse,
    /// A write naming a real account (or an id shape we cannot parse — see [`account_gate`]).
    Proceed,
}

/// Is this method one that can WRITE (and therefore bind `claims.sub` into an FK column)?
///
/// POST/PUT/PATCH/DELETE only. `GET`/`HEAD`/`OPTIONS` cannot violate an FK — they never insert —
/// and refusing them would turn every stale token's empty dashboard into an error.
pub(crate) fn is_state_changing(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// The guard's decision, independent of the database.
pub(crate) fn account_gate(method: &Method, account_id_names_a_row: bool) -> AccountGate {
    if !is_state_changing(method) {
        AccountGate::Skip
    } else if account_id_names_a_row {
        AccountGate::Proceed
    } else {
        AccountGate::Refuse
    }
}

/// The 4xx a state-changing request gets when its token names an account that does not exist.
///
/// 404 (not 500, and not a blanket 403): the caller learns WHICH thing was wrong. The message
/// deliberately does not echo the id — the caller already has it in its own token, and echoing it
/// would put an arbitrary value into the response body.
pub(crate) fn unknown_account() -> AppError {
    AppError::NotFound(
        "Unknown account: this token names an account that does not exist".to_string(),
    )
}

/// The one user-visible string for a credential whose `accounts` row is gone (kanban t_53fcb666).
/// The SPA's 401 arm turns it into "Session expired" + sign-out.
pub(crate) const SESSION_ENDED: &str = "This session is no longer valid. Please sign in again.";

/// The 401 a READ gets when its credential names an `accounts` row that no longer exists.
pub(crate) fn session_ended() -> AppError {
    AppError::Unauthorized(SESSION_ENDED.to_string())
}

/// The refusal owed to a request whose credential names no `accounts` row, or `None` when the row
/// is there (or the method is a read of a live account, which pays no refusal).
///
/// Two shapes, both 4xx, decided here so both arms are unit-tested:
/// * a STATE-CHANGING request keeps the field-level 404 it has always answered (the caller learns
///   WHICH thing was wrong, and it is not a 500 — see the module docs);
/// * a READ has nothing to write, so there is no FK to violate — it answers **401**, which is what
///   makes deleting the row BE the revocation (kanban t_53fcb666). Measured on this app 2026-10-20
///   BEFORE this: after `delete from accounts`, the same token still answered `200` from
///   `GET /api/v1/credits/balance` and `GET /api/v1/campaigns` (only `/auth/me` 404'd, because
///   that handler looks the row up itself). The token lives 30 days, so deletion has to revoke.
pub(crate) fn refusal_for(method: &Method, names_a_row: bool) -> Option<AppError> {
    match account_gate(method, names_a_row) {
        AccountGate::Proceed => None,
        AccountGate::Refuse => Some(unknown_account()),
        // A read (the method cannot write) whose account is gone: 401, never a quiet 200.
        AccountGate::Skip => {
            if names_a_row {
                None
            } else {
                Some(session_ended())
            }
        }
    }
}

/// What kind of credential a request presented, for the `sessions_valid_from` arm.
///
/// Only a browser SESSION can be ended by a password change. A durable API key is an integration
/// credential the account issued on purpose; measuring it against the password timestamp would
/// silently revoke every integration the moment the owner rotated a password, so it is never
/// measured at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssuedAt {
    /// A session JWT and its `iat` (seconds since the epoch, when the mint recorded one).
    Session(Option<i64>),
    /// A durable, non-session credential (an issued API key).
    Durable,
}

/// Is a JWT older than the instant its account last changed or reset its password?
///
/// Pure, so both arms are unit-tested and the rule is readable without a database. `valid_from`
/// is `accounts.sessions_valid_from` as a unix epoch (`None` there = the account has never
/// restricted its sessions, which is every account until it changes or resets its password, so
/// nothing changes for tokens already in the wild). A session with NO `iat` cannot prove it is
/// newer than the change, and the safe answer for a credential is "cannot prove → refuse". `iat` is second-granularity (and the change's own timestamp is floored to a second for this reason), so a session minted in the SAME second as the change cannot be told apart from a re-login and is allowed.
/// A [`IssuedAt::Durable`] credential is not a session and is never refused here.
pub(crate) fn token_predates_password_change(issued: IssuedAt, valid_from: Option<i64>) -> bool {
    let IssuedAt::Session(jwt_iat) = issued else {
        return false;
    };
    match valid_from {
        None => false,
        Some(v) => match jwt_iat {
            None => true,
            Some(iat) => iat < v,
        },
    }
}

/// Refuse a request whose token names an `accounts` row that does not exist.
///
/// One existence lookup, at the boundary, before any handler runs — which is what closes the whole
/// class at once: every writer that binds `claims.sub` into an FK column (`tenant_settings.
/// tenant_id`, `tags.account_id`, `provider_keys.account_id`, …) takes `AuthenticatedUser`, so
/// there is no path around this check. A shape that is not a uuid at all cannot name a row either
/// (`credits_handler` used to bind `Uuid::nil()` for one, which is just as absent), so it is
/// refused the same way — still a 4xx, never a 500.
///
/// Since kanban t_53fcb666 the lookup runs for READS too (one primary-key read), because a token
/// whose account is gone must not keep reading: the refusal there is 401 (see [`refusal_for`]).
///
/// SESSION REVOCATION (2026-10-20): the same single read also returns
/// `accounts.sessions_valid_from`, so a JWT minted before the account's last password change or
/// reset is refused here with [`SESSION_ENDED`] — no second query. Measured before this: a token
/// minted before a password reset still answered 200 afterwards, i.e. changing the password (the
/// one thing a user does when they think their session is stolen) revoked nothing.
async fn guard_account_exists(
    db: &sqlx::PgPool,
    method: &Method,
    account_id: &str,
    issued: IssuedAt,
) -> Result<(), AppError> {
    // ONE read answers both questions: does the row exist, and (if it exists) when were its
    // sessions last invalidated. `COALESCE(..., 0)` keeps a NULL — the ordinary case — as "no
    // restriction" without a second column read.
    let valid_from = match uuid::Uuid::parse_str(account_id) {
        Ok(id) => {
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(floor(EXTRACT(EPOCH FROM sessions_valid_from))::bigint, 0)
                   FROM accounts WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(db)
            .await?
        }
        Err(_) => None,
    };
    let names_a_row = valid_from.is_some();
    if names_a_row && token_predates_password_change(issued, valid_from.filter(|v| *v > 0)) {
        return Err(session_ended());
    }
    match refusal_for(method, names_a_row) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Resolve a bearer token that claims to be an IncentiveSwift API key.
///
/// Two credential stores exist and both are consulted, `api_keys` first:
///
/// 1. `api_keys` — what `POST /api/v1/api-keys` writes (the admin + tenant
///    "API Keys" screens). Keys are `is_key_<48 random alphanumerics>`; the stored
///    `prefix` is the first 8 characters of the random part and `key_hash` is a
///    bcrypt hash of the WHOLE key.
/// 2. `api_credentials` — legacy rows (`key_identifier` + bcrypt `key_hash`).
///
/// Returns `Ok(None)` when the token is not an API key at all, so the caller can
/// fall back to JWT validation; `Err(Unauthorized)` when it looks like one but the
/// secret does not match (or the key is deactivated/expired).
///
/// This is the single resolver behind BOTH `AuthenticatedUser` and
/// `POST /api/v1/api-keys/verify`, which is what makes the verify endpoint honest:
/// a key that verifies is a key that really authenticates.
pub(crate) async fn validate_api_key(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    if let Some(user) = verify_issued_api_key(state, token).await? {
        return Ok(Some(user));
    }
    verify_legacy_api_credential(state, token).await
}

/// Issued keys are `is_key_<48 random alphanumerics>`; `api_keys.prefix` stores the
/// first 8 characters of the random part.
const ISSUED_KEY_PREFIX: &str = "is_key_";
const KEY_PREFIX_LEN: usize = 8;

/// The `api_keys.prefix` value for a presented token, or `None` when the token is not
/// an issued key at all.
///
/// Byte-safe on purpose: `str::get` refuses to slice inside a multi-byte character, so
/// a hand-crafted token (`is_key_aéééé`) cannot panic the request — a real panic here
/// was reachable unauthenticated on `POST /api/v1/api-keys/verify`. Such a token simply
/// is not a key; anything non-ASCII can never match a stored prefix (they are alnum).
fn issued_key_prefix(token: &str) -> Option<&str> {
    token
        .strip_prefix(ISSUED_KEY_PREFIX)
        .and_then(|secret| secret.get(..KEY_PREFIX_LEN))
}

/// Look a key up in `api_keys` (the store the CRUD API and the UI write).
///
/// `Ok(None)` = prefix unknown, not an issued key (caller may try the legacy store).
/// `Err(Unauthorized)` = the key exists but is inactive, expired, or the secret is wrong.
async fn verify_issued_api_key(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    let Some(prefix) = issued_key_prefix(token) else {
        return Ok(None);
    };

    // The column has no unique constraint, so verify against every row that shares
    // the prefix rather than assuming the first one is the key.
    let rows = sqlx::query(
        "SELECT ak.key_hash, ak.user_id::text AS user_id, ak.is_active, ak.expires_at,
                COALESCE(a.email, '') AS email
           FROM api_keys ak
           LEFT JOIN accounts a ON a.id = ak.user_id
          WHERE ak.prefix = $1",
    )
    .bind(prefix)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    for r in &rows {
        let is_active: bool = r.try_get("is_active").unwrap_or(false);
        if !is_active {
            continue;
        }
        let expires_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("expires_at")?;
        if expires_at.is_some_and(|e| e <= chrono::Utc::now()) {
            continue;
        }

        let stored_hash: String = r.get("key_hash");
        // bcrypt::verify is the only correct comparison for a stored key hash.
        if bcrypt::verify(token, &stored_hash).unwrap_or(false) {
            return Ok(Some(AuthenticatedUser {
                account_id: r.get("user_id"),
                email: r.get("email"),
                role: "api_key".to_string(),
                impersonating: None,
            }));
        }
    }

    Err(AppError::Unauthorized("Invalid API key".to_string()))
}

/// Legacy store: `api_credentials.key_identifier` + bcrypt hash. Unchanged
/// behaviour — kept so any credential seeded outside `api_keys` keeps working.
async fn verify_legacy_api_credential(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    // Extract key identifier (first 8 chars of the key, or use a prefix scheme)
    // API keys should be formatted as: "is_key_<identifier>_<secret>"
    let parts: Vec<&str> = token.split('_').collect();
    if parts.len() < 3 || parts[0] != "is" || parts[1] != "key" {
        return Ok(None);
    }

    let identifier = parts[1..parts.len() - 1].join("_");
    let _secret = parts.last().unwrap_or(&"");

    // Look up the stored hash by identifier
    let row = sqlx::query(
        "SELECT ac.key_hash, ac.account_id::text, a.email
         FROM api_credentials ac
         JOIN accounts a ON a.id = ac.account_id
         WHERE ac.key_identifier = $1",
    )
    .bind(&identifier)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some(r) => {
            let stored_hash: String = r.get("key_hash");
            let account_id: String = r.get("account_id");
            let email: String = r.get("email");

            // Verify with bcrypt — this is the correct way
            // bcrypt::verify is intentionally slow and salted
            match bcrypt::verify(token, &stored_hash) {
                Ok(true) => Ok(Some(AuthenticatedUser {
                    account_id,
                    email,
                    role: "api_key".to_string(),
                    impersonating: None,
                })),
                _ => Err(AppError::Unauthorized("Invalid API key".to_string())),
            }
        }
        None => Ok(None), // Not an API key, try JWT validation
    }
}

// Fleet guard for the operator-only surfaces: `/api/v1/admin/*` and `OPERATOR_ONLY_PATHS`.
//
// SECURITY (2026-09-20): these routes previously answered 2xx to *anonymous*
// callers (12 of them, including the state-changing
// `POST /api/v1/admin/treasury/expire-points`). This middleware is applied
// globally but only inspects operator paths, so tenant traffic is untouched.
//
// Allowed callers:
// - a valid JWT or API key whose role is `admin` / `super_admin`;
// - a sibling service presenting the shared `X-Internal-Sync-Key`.
//
// Route families that only a platform operator (`admin` / `super_admin`) may reach.
//
// The `/api/v1/admin` prefix is the app's long-standing operator surface, but it is not the ONLY
// one: the served console (`www-admin/index.html`) marks six nav entries `adminOnly: true`, and
// three of the screens behind them read routes that live OUTSIDE the prefix. Measured live
// 2026-10-02 (kanban t_88e535af), a token minted for the same account at role `company_admin`
// answered **200** on all of these while `/api/v1/admin/*` correctly answered **403**:
//
// ```text
// GET /api/v1/plans                         200   <- LEFT OPEN, deliberately (see below)
// GET /api/v1/portfolio-companies           200
// GET /api/v1/email-templates               200
// GET /api/v1/email-templates/merge-fields  200
// ```
const OPERATOR_ONLY_PATHS: &[&str] = &["/api/v1/portfolio-companies", "/api/v1/email-templates"];

/// Is `path` on an operator-only surface?
///
/// Segment match (`exact` or `exact/...`), not a string prefix: `/api/v1/email-templatesX` is a
/// different route and must not be caught. `format!`-free on purpose — this runs on every request
/// of a ~700-route router.
///
/// `/api/v1/plans` is deliberately NOT in the set: it is the public plan catalogue
/// (`handlers::dashboard_handler::list_public_plans`, doc-commented "public - no auth required",
/// anonymous 200 live) with no caller-scoped data, and the fleet's other apps expose the same
/// path as the self-serve upgrade listing. The console's `plans` SCREEN is still operator-only:
/// its list and CRUD all go through `/api/v1/admin/plans`, which this guard already refuses.
pub fn is_admin_surface(path: &str) -> bool {
    if path == "/api/v1/admin" || path.starts_with("/api/v1/admin/") {
        return true;
    }
    OPERATOR_ONLY_PATHS.iter().any(|base| {
        path.strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// The ONE operator path an impersonation session must be able to reach: the one that ends it.
const ADMIN_STOP_IMPERSONATION: &str = "/api/v1/admin/stop-impersonation";

/// The roles the operator-only surfaces admit — ONE list, ONE predicate, so the guard's decision
/// and the unit tests can never drift apart.
pub fn is_platform_admin(role: &str) -> bool {
    role == "admin" || role == "super_admin"
}

/// True when `path` is the one operator route an impersonation session must be able to call.
pub fn is_stop_impersonation(path: &str) -> bool {
    path == ADMIN_STOP_IMPERSONATION
}

/// The full operator-surface decision, including the single carve-out an impersonation session
/// needs: `true` means answer 403.
///
/// [`is_admin_surface`] stays a pure path test. This adds the one exception that has to exist:
/// `handlers::admin_handler::impersonate` mints the impersonation JWT with the IMPERSONATED
/// account's own role (`target_role`), so every `/api/v1/admin/*` path is refused to it — including
/// `POST /api/v1/admin/stop-impersonation`, whose entire purpose is to end that session. Measured
/// live on admin.incentiveswift.com (kanban t_abecd334): the impersonation session's own Stop
/// answered `403 Admin role required`, so an operator could only abandon the session by dropping the
/// token. The carve-out is exactly ONE path and it requires the `impersonating` claim that only the
/// impersonate handler sets, so a plain tenant token gains nothing from it or from any sibling.
pub fn admin_surface_denied_for(path: &str, role: &str, impersonating: bool) -> bool {
    if impersonating && is_stop_impersonation(path) {
        return false;
    }
    is_admin_surface(path) && !is_platform_admin(role)
}

/// Auth guard for the operator-only surfaces: `/api/v1/admin/*` and [`OPERATOR_ONLY_PATHS`].
///
/// SECURITY (kanban t_88e535af). Until this pass the console's `adminOnly` nav flag was the ONLY
/// thing hiding those three screens from a tenant admin: hiding a nav entry does not hide the
/// surface from a token, so a `company_admin` could call the routes directly. The gate is
/// method-agnostic and path-based, so one call per path proves it for every verb.
pub async fn admin_guard(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    let path = req.uri().path().to_string();
    if !is_admin_surface(&path) {
        return Ok(next.run(req).await);
    }

    // Sibling-bot / cron bypass using the shared internal sync key.
    //
    // SECURITY (kanban t_bc20ec78, measured live 2026-10-02). This branch used to return early
    // whenever the key matched — BEFORE the Authorization header was ever looked at. Handlers on
    // this surface that take an `AuthenticatedUser` then accepted *any* valid JWT, so a borrowed
    // non-operator token inherited the key's authority: measured on the pre-fix binary,
    // `POST /api/v1/admin/portfolio-sync` with `X-Internal-Sync-Key: <key>` + a `company_admin`
    // JWT answered **200** and returned every account's portfolio companies (the same shape on
    // `GET /api/v1/admin/tenants` and `GET /api/v1/admin/credits`). The key alone answers 401
    // there (the extractor needs a credential), so the key-holder's real credential was the
    // borrowed tenant token.
    //
    // The bypass is therefore narrowed to what it was written for — a service call, i.e. a
    // request that presents NO Authorization header. A request that presents a JWT is still held
    // to the operator role, so a tenant token can no longer ride the key. Key-only callers are
    // unchanged (`GET /api/v1/admin/treasury/summary` 200 before and after; on the two routes
    // that need a credential it is 401 before and after).
    let sync_key = state.config.internal_sync_key.clone();
    if !sync_key.is_empty() && req.headers().get("Authorization").is_none() {
        let presented = req
            .headers()
            .get("X-Internal-Sync-Key")
            .and_then(|v| v.to_str().ok());
        if presented == Some(sync_key.as_str()) {
            return Ok(next.run(req).await);
        }
    }

    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(|| AppError::Unauthorized("Admin authentication required".to_string()))?;

    // The role AND the impersonation claim decide together: the claim is what lets an impersonation
    // session reach the ONE route that ends it (see [`admin_surface_denied_for`]). An API key
    // resolves to a role but carries no claim, so it is never impersonating.
    //
    // `subject` is the `accounts` id the credential names — `None` on the API-key arm, whose role is
    // the literal `api_key` and whose row is the key's own. It is needed by the demotion check
    // below, which compares the CLAIM to the row.
    let (role, impersonating, subject) = match validate_api_key(&state, &token).await? {
        Some(user) => (user.role, false, None),
        None => {
            let claims = crate::security::jwt::verify_jwt(&token, &state.config.jwt_secret)?;
            (
                claims
                    .role
                    .clone()
                    .unwrap_or_else(|| "authenticated".to_string()),
                claims.impersonating.is_some(),
                claims.sub.clone(),
            )
        }
    };

    if admin_surface_denied_for(&path, &role, impersonating) {
        return Err(AppError::Forbidden("Admin role required".to_string()));
    }

    // ── the CLAIM must not exceed what the live `accounts` row grants (kanban t_28184826) ────────
    // Measured live on funnelswift 2026-10-20 after kanban t_53fcb666 shipped: deleting an account
    // revoked its token, but DEMOTING an operator (the other half of "remove the compromised
    // account") did not — the guard above decided on `claims.role` alone. This app's platform
    // authority lives on the row that the login reads (`accounts.role`, minted into the token by
    // `handlers::auth_handler::create_jwt`), so a 30-day token minted while the row said `admin`
    // kept the whole operator surface after an `UPDATE accounts SET role='company_admin'`.
    //
    // One-directional on purpose: a claim LOWER than the row is not this rule's business, the
    // impersonation session (minted with the TARGET's own role) is never a platform-admin claim,
    // and an API key's `api_key` role is not one either — so every tenant surface keeps the
    // behaviour it had. Fail closed: an unparseable or missing subject, and a row that no longer
    // grants the role, both refuse 401; a database that cannot answer is a 5xx, never a pass.
    if is_platform_admin(&role) {
        let Some(sub) = subject.as_deref() else {
            return Err(AppError::Unauthorized(
                "This session is no longer valid. Please sign in again.".to_string(),
            ));
        };
        match account_role(&state.db, sub).await {
            Ok(row_role) => {
                if !claim_backed_by_row(&role, row_role.as_deref()) {
                    tracing::warn!(
                        subject = %sub,
                        "refused an admin role claim the accounts row no longer grants"
                    );
                    return Err(AppError::Unauthorized(
                        "This session is no longer valid. Please sign in again.".to_string(),
                    ));
                }
            }
            Err(e) => {
                tracing::error!("account role lookup failed for {sub}: {e}");
                return Err(AppError::Internal(
                    "Admin authorisation temporarily unavailable".to_string(),
                ));
            }
        }
    }

    Ok(next.run(req).await)
}

/// Is a platform-admin CLAIM still backed by the `accounts` row?
///
/// `true` when the credential claims no platform authority at all (that is not this rule's
/// business — tenant surfaces and `api_key`/impersonation sessions are untouched), and `true` when
/// the row still grants it. `false` — the refusal — when the claim is platform-admin and the row is
/// missing, unreadable as an id, or has been DEMOTED (kanban t_28184826).
pub(crate) fn claim_backed_by_row(claim_role: &str, row_role: Option<&str>) -> bool {
    !is_platform_admin(claim_role) || row_role.is_some_and(is_platform_admin)
}

/// The live `role` of the `accounts` row named by a token's `sub`, or `None` when it is gone or the
/// id does not parse. `Err` only when the database could not answer, so a broken lookup is a 5xx
/// rather than a misleading 401 (kanban t_28184826).
async fn account_role(db: &sqlx::PgPool, sub: &str) -> Result<Option<String>, sqlx::Error> {
    let Ok(account_id) = uuid::Uuid::parse_str(sub) else {
        return Ok(None);
    };
    sqlx::query_scalar::<_, String>("SELECT role FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::{
        account_gate, admin_surface_denied_for, claim_backed_by_row, is_admin_surface,
        is_platform_admin, is_state_changing, is_stop_impersonation, issued_key_prefix,
        refusal_for, session_ended, token_predates_password_change, unknown_account, AccountGate,
        IssuedAt, KEY_PREFIX_LEN, SESSION_ENDED,
    };
    use crate::error::AppError;
    use axum::http::Method;

    /// The reported defect (kanban t_28184826): the operator guard proved the credential was a
    /// real signed token, but decided on `claims.role` alone. This app mints the role from
    /// `accounts.role` at login (30-day JWT) and never re-reads it, so DEMOTING an operator —
    /// `UPDATE accounts SET role='company_admin'` — left the old token on the whole operator
    /// surface. A platform-admin claim the live row no longer backs must be refused; every claim
    /// that carries no platform authority (`user`, `company_admin`, the API key's `api_key`, the
    /// impersonation session's target role) is untouched.
    #[test]
    fn a_platform_admin_claim_the_accounts_row_no_longer_grants_is_refused() {
        // demoted rows: claim says admin/super_admin, row says something lower
        for claim in ["admin", "super_admin"] {
            for demoted_to in ["company_admin", "business_user", "user", ""] {
                assert!(
                    !claim_backed_by_row(claim, Some(demoted_to)),
                    "a {claim} claim over a {demoted_to:?} row must be refused"
                );
            }
            // …and a row that is GONE backs nothing either.
            assert!(!claim_backed_by_row(claim, None));
        }
        // A real operator is untouched, and either admitted role backs either claim.
        for claim in ["admin", "super_admin"] {
            for row in ["admin", "super_admin"] {
                assert!(claim_backed_by_row(claim, Some(row)));
            }
        }
        // Non-operator claims are NEVER refused by this rule — this is what keeps every tenant
        // surface (and the api_key / impersonation doors) exactly as it was.
        for claim in [
            "user",
            "company_admin",
            "business_user",
            "api_key",
            "authenticated",
            "",
        ] {
            assert!(claim_backed_by_row(claim, Some("company_admin")));
            assert!(claim_backed_by_row(claim, Some("user")));
            assert!(claim_backed_by_row(claim, None));
        }
        // A claim LOWER than the row is not this rule's business either.
        assert!(claim_backed_by_row("company_admin", Some("admin")));
        assert!(claim_backed_by_row("user", Some("super_admin")));
    }

    /// The request-entry account guard (kanban t_a7b7b5b9), reduced to its pure decision: a
    /// STATE-CHANGING method naming no `accounts` row is refused; a write naming a real account
    /// proceeds; a read is left exactly as it was (a gone account's GET is an empty list, never a
    /// 500 and never a refusal).
    #[test]
    fn account_guard_refuses_writes_for_an_unknown_account_and_leaves_reads_alone() {
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_state_changing(&m), "{m} can write");
            assert_eq!(account_gate(&m, false), AccountGate::Refuse, "{m} unknown");
            assert_eq!(account_gate(&m, true), AccountGate::Proceed, "{m} known");
        }
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_state_changing(&m), "{m} cannot write");
            assert_eq!(account_gate(&m, false), AccountGate::Skip, "{m} unknown");
            assert_eq!(account_gate(&m, true), AccountGate::Skip, "{m} known");
        }
    }

    /// The read arm of the SAME guard (kanban t_53fcb666). Before this, a token whose `accounts`
    /// row had been deleted still answered 200 from `GET /api/v1/credits/balance` and
    /// `GET /api/v1/campaigns` — the credential outlived the account for its full 30 days. Both
    /// directions are pinned: a read naming a LIVE account is untouched (no refusal, so the guard
    /// is not a blanket gate), and a read naming nobody is 401 with the SPA's own sign-out string.
    #[test]
    fn a_read_naming_a_deleted_account_is_refused_401_and_a_live_one_is_untouched() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(
                refusal_for(&m, true).is_none(),
                "{m} live account must pass"
            );
            match refusal_for(&m, false) {
                Some(AppError::Unauthorized(msg)) => {
                    assert_eq!(msg, SESSION_ENDED, "the SPA matches this string")
                }
                other => panic!("{m} gone account must be 401, got {other:?}"),
            }
        }
        // The write arm is unchanged by this card: still the field-level 404.
        assert!(matches!(
            refusal_for(&Method::POST, false),
            Some(AppError::NotFound(_))
        ));
        assert!(refusal_for(&Method::POST, true).is_none());
        assert!(matches!(session_ended(), AppError::Unauthorized(_)));
    }

    /// The refusal is the app's own 4xx and NAMES the account — the whole point of moving it off
    /// the database, where the same request answered `500 Internal server error`.
    #[test]
    fn unknown_account_is_a_404_that_names_the_account() {
        match unknown_account() {
            AppError::NotFound(msg) => assert!(msg.contains("account"), "{msg}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// The operator-only surface set (kanban t_88e535af). Every path here answered 200 to a
    /// `company_admin` token before the guard covered it; each must now be refused, and the
    /// deliberate NON-members must not be swept up by an over-broad match.
    #[test]
    fn operator_surfaces_include_the_three_console_screens_that_are_not_prefixed() {
        for p in [
            "/api/v1/admin/site",
            "/api/v1/admin",
            "/api/v1/admin/",
            // the console's `adminOnly` screens outside the /admin prefix (measured live).
            // The id segments are arbitrary on purpose: the guard matches a path SEGMENT, so the
            // suffix is what is under test — and the deploy gate forbids UUID literals in src/.
            "/api/v1/portfolio-companies",
            "/api/v1/portfolio-companies/0a1b2c3d",
            "/api/v1/email-templates",
            "/api/v1/email-templates/0a1b2c3d",
            "/api/v1/email-templates/merge-fields",
        ] {
            assert!(is_admin_surface(p), "{p} must be operator-only");
        }
    }

    /// The other side of the same rule: these must stay reachable by a tenant token.
    /// `/api/v1/plans` is the PUBLIC catalogue (anonymous 200 live, no caller-scoped data) and the
    /// tenant surfaces are the positive control — a blanket gate is this card's stated failure mode.
    #[test]
    fn public_and_tenant_surfaces_are_not_operator_only() {
        for p in [
            "/api/v1/plans",
            "/api/v1/dashboard/stats",
            "/api/v1/leads",
            "/api/v1/tags",
            "/api/v1/campaigns",
            "/api/v1/settings",
            "/api/v1/integration-targets",
            "/api/v1/internal/portfolio-companies",
            // segment match, not string prefix: neither of these is the gated route
            "/api/v1/email-templatesX",
            "/api/v1/portfolio-companies-archive",
            "/api/v1/administrators",
        ] {
            assert!(!is_admin_surface(p), "{p} must stay reachable");
        }
    }

    /// The live defect (kanban t_abecd334): an impersonation session could START but not STOP.
    /// `handlers::admin_handler::impersonate` mints the impersonation JWT with the TARGET account's
    /// own role, so the token an operator holds while impersonating is never `admin` and the guard
    /// refused every `/api/v1/admin/*` path — `stop-impersonation` included, before its handler ran.
    ///
    /// Both directions of the one carve-out, and the non-impersonation arms, are pinned here so a
    /// future widening has to change this test too.
    #[test]
    fn an_impersonation_session_may_end_itself_and_nothing_else_on_the_operator_surface() {
        let stop = "/api/v1/admin/stop-impersonation";
        // The one carve-out: an impersonation token reaches the route that ends it...
        assert!(!admin_surface_denied_for(stop, "company_admin", true));
        assert!(!admin_surface_denied_for(stop, "user", true));
        assert!(is_stop_impersonation(stop));
        // ...and still reaches NOTHING else on the operator surface — the prefix is not a door.
        for path in [
            "/api/v1/admin",
            "/api/v1/admin/impersonate",
            "/api/v1/admin/tenants",
            "/api/v1/admin/portfolio-sync",
            "/api/v1/admin/allcampaigns",
            "/api/v1/portfolio-companies",
            "/api/v1/email-templates",
        ] {
            assert!(
                admin_surface_denied_for(path, "company_admin", true),
                "an impersonation token must not reach {path}"
            );
        }
        // A NON-impersonation tenant token gains nothing from the carve-out: that is the whole point
        // of keying on the `impersonating` claim rather than on the path alone.
        assert!(admin_surface_denied_for(stop, "company_admin", false));
        assert!(admin_surface_denied_for(stop, "user", false));
        // The platform admin keeps every door, claim or not.
        assert!(!admin_surface_denied_for(stop, "admin", false));
        assert!(!admin_surface_denied_for(stop, "super_admin", false));
        assert!(!admin_surface_denied_for(
            "/api/v1/portfolio-companies",
            "admin",
            false
        ));
        // The carve-out is ONE exact path, not a prefix: no sibling rides in on it.
        for near in [
            "/api/v1/admin/stop-impersonation/anything",
            "/api/v1/admin/stop-impersonate",
            "/api/v1/admin/stop-impersonation/",
        ] {
            assert!(
                admin_surface_denied_for(near, "company_admin", true),
                "{near} must not ride the stop-impersonation carve-out"
            );
        }
        // One role list, and it is the narrow one the console's admin surface uses.
        assert!(is_platform_admin("admin"));
        assert!(is_platform_admin("super_admin"));
        for role in [
            "company_admin",
            "user",
            "owner",
            "impersonated",
            "ADMIN",
            "",
        ] {
            assert!(!is_platform_admin(role), "{role} is not a platform admin");
        }
    }

    /// The column `POST /api/v1/api-keys` fills: first 8 chars of the random part.
    #[test]
    fn prefix_is_the_first_eight_of_the_random_part() {
        let key = format!("is_key_{}", "aB3xY7zQ".to_owned() + &"k".repeat(40));
        assert_eq!(issued_key_prefix(&key), Some("aB3xY7zQ"));
        assert_eq!(issued_key_prefix(&key).unwrap().len(), KEY_PREFIX_LEN);
    }

    #[test]
    fn short_and_foreign_tokens_are_not_keys() {
        assert_eq!(issued_key_prefix("is_key_abc"), None);
        assert_eq!(issued_key_prefix("is_key_"), None);
        assert_eq!(issued_key_prefix(""), None);
        assert_eq!(issued_key_prefix("eyJhbGciOiJIUzI1NiJ9.e30.sig"), None);
        assert_eq!(issued_key_prefix("IS_KEY_abcdefgh"), None);
    }

    /// Regression: `&secret[..8]` panicked here ("byte index 8 is not a char boundary"),
    /// reachable unauthenticated on POST /api/v1/api-keys/verify (live 2026-09-21).
    #[test]
    fn multibyte_token_is_rejected_without_panicking() {
        // 'a' + 4x 'é': byte 8 falls inside a character — exactly the live panic
        assert_eq!(issued_key_prefix("is_key_aéééé"), None);
        // 8 bytes of multi-byte characters: index 8 is the end of the string, a valid
        // boundary, so this is a (never-matching) value rather than a panic
        assert_eq!(issued_key_prefix("is_key_éééé"), Some("éééé"));
        // mixed: 3x 'é' is 6 bytes, so the 8-byte window closes on boundaries
        assert_eq!(issued_key_prefix("is_key_éééab"), Some("éééab"));
    }

    /// The reported defect (kanban t_80787a0f): changing or resetting the password revoked
    /// nothing, because the boundary decided on the token's claims alone. The rule is now one
    /// comparison — a session minted BEFORE the account's `sessions_valid_from` is refused — and
    /// it must change nothing else: a NULL timestamp (every account that never rotated), a
    /// session minted at or after the change, and a durable API key all stay exactly as they were.
    #[test]
    fn a_session_minted_before_the_password_change_is_refused_and_nothing_else_is() {
        // NULL column: never restricted, so every token already in the wild keeps working.
        assert!(!token_predates_password_change(
            IssuedAt::Session(Some(1_000)),
            None
        ));
        // Minted BEFORE the change -> refused.
        assert!(token_predates_password_change(
            IssuedAt::Session(Some(1_000)),
            Some(2_000)
        ));
        // Minted AT the change or after it -> allowed (a re-login must work).
        assert!(!token_predates_password_change(
            IssuedAt::Session(Some(2_000)),
            Some(2_000)
        ));
        assert!(!token_predates_password_change(
            IssuedAt::Session(Some(3_000)),
            Some(2_000)
        ));
        // A session that cannot prove it is newer is refused rather than trusted.
        assert!(token_predates_password_change(
            IssuedAt::Session(None),
            Some(2_000)
        ));
        assert!(!token_predates_password_change(
            IssuedAt::Session(None),
            None
        ));
        // A durable API key is an integration credential, not a session: never revoked by this.
        assert!(!token_predates_password_change(
            IssuedAt::Durable,
            Some(2_000)
        ));
    }
}
