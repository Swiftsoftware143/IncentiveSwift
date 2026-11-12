//! The session claims the ported private-inbox and workspace modules read.
//!
//! IncentiveSwift's credential boundary is the [`AuthenticatedUser`] extractor
//! (`crate::security::auth`), which answers `agent_id`/`email`/`role` for a Bearer JWT or an issued
//! API key. The inbox and workspace modules were ported from ADASwift, where the boundary is a
//! middleware that inserts a `Claims` extension and the handlers read `claims.aid`.
//!
//! Rather than rewrite ~60 handler signatures, the SAME boundary is expressed here as a middleware
//! that runs the app's own extractor and inserts one [`Claims`] extension — so the ported handlers
//! keep their shape while every rule (JWT signature, session version, account existence, API keys)
//! stays in the ONE place this app already enforces it.
//!
//! `aid` is the `accounts` row id: in this app the account IS the tenant (`accounts.tenant_id` is
//! the row's own id for a self-serve signup — see `crate::account_mint`).

use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// The claims shape the ported modules consume.
#[derive(Debug, Clone)]
pub struct Claims {
    /// The `accounts` row id — what the ported SQL binds as `tenant_id`.
    pub aid: String,
    /// Same value, kept under the name the JWT itself uses.
    pub sub: String,
    pub email: String,
    pub role: String,
}

/// Authenticate the request with the app's own extractor and expose it as [`Claims`].
pub async fn claims_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let (mut parts, body) = req.into_parts();
    let user = AuthenticatedUser::from_request_parts(&mut parts, &state).await?;
    let claims = Claims {
        aid: user.account_id.clone(),
        sub: user.account_id,
        email: user.email,
        role: user.role,
    };
    let mut req = Request::from_parts(parts, body);
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

impl Claims {
    /// The account id as a UUID, or the 401 the ported handlers answer for an unparseable session.
    pub fn account_uuid(&self) -> Result<uuid::Uuid, AppError> {
        uuid::Uuid::parse_str(&self.aid)
            .map_err(|_| AppError::UnauthorizedReason("Invalid session".to_string()))
    }
}
