//! Platform billing settings (admin): how a customer pays while card payments are off.
//!
//! Routes:
//!   GET /api/v1/admin/billing-settings — the operator's saved words (empty string = feature off)
//!   PUT /api/v1/admin/billing-settings — save them (`{"payment_instructions": "..."}`)
//!
//! `/api/v1/admin/*` is covered by `security::auth::admin_guard` (platform operator only), so a
//! tenant owner is refused here exactly like every other admin route.
//!
//! Why this exists: with no `payment_providers` row and no Stripe key, `POST /api/v1/checkout/create`
//! refuses every paid plan and the plan REQUEST is the buying path. The customer was told "a person
//! will reply" and never how to actually pay — FunnelSwift closed the same gap on 2026-11-16; this
//! is the IncentiveSwift side of that parity.
//!
//! The stored value is read in three places, all through `billing::instructions::load`:
//! the customer till status (`GET /api/v1/checkout/status`), the plan-request receipt mail, and
//! this screen.

use crate::billing::instructions;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

/// GET /api/v1/admin/billing-settings
pub async fn get_billing_settings(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let saved = instructions::load(&state.db).await;
    Ok(Json(json!({
        "payment_instructions": saved.clone().unwrap_or_default(),
        "payment_instructions_set": saved.is_some(),
        "max_length": instructions::MAX_LEN,
        "message": if saved.is_some() {
            "A customer who asks to buy is shown these words, and they are printed in the receipt email."
        } else {
            "Nothing is shown to a customer who asks to buy, and the receipt email is unchanged."
        },
    })))
}

#[derive(Deserialize)]
pub struct BillingSettingsBody {
    /// The operator's own words. Absent keeps the stored value; an empty string CLEARS it (the
    /// feature goes back to off) — the same three-state contract the sibling consoles use.
    pub payment_instructions: Option<String>,
}

/// PUT /api/v1/admin/billing-settings
pub async fn update_billing_settings(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
    Json(body): Json<BillingSettingsBody>,
) -> Result<Json<Value>, AppError> {
    let Some(text) = body.payment_instructions else {
        return Err(AppError::BadRequest(
            "Send {\"payment_instructions\": \"...\"}. An empty string clears it.".to_string(),
        ));
    };
    instructions::save(&state.db, &text)
        .await
        .map_err(AppError::BadRequest)?;

    let saved = instructions::load(&state.db).await;
    Ok(Json(json!({
        "success": true,
        "payment_instructions": saved.clone().unwrap_or_default(),
        "payment_instructions_set": saved.is_some(),
    })))
}
