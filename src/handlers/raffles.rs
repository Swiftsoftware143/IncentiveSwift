//! Raffle handlers — enter, draw, redraw.

use crate::db::{campaigns, contacts, raffles};
use crate::error::AppError;
use crate::handlers::campaigns::campaign_for_caller;
use crate::mechanics::raffle_draw;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Body for entering a raffle.
#[derive(Deserialize)]
pub struct EnterRaffleBody {
    pub contact: super::entries::ContactBody,
    pub consent_gathered: bool,
    pub answers: Option<Value>,
    /// The referral code this visitor arrived with (`?ref=` on a shared campaign link). The served
    /// play page already sends it inside its payload; before t_6723eb30 this handler simply
    /// ignored it, so a raffle participant got no share link and a friend arriving through one was
    /// never credited.
    #[serde(rename = "ref", default)]
    pub referral_code: Option<String>,
}

/// POST /api/v1/raffles/:slug/enter — public.
pub async fn enter_raffle(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<EnterRaffleBody>,
) -> Result<Json<Value>, AppError> {
    // Validate consent
    if !body.consent_gathered {
        return Err(AppError::BadRequest(
            "consent_gathered must be true to enter raffle".to_string(),
        ));
    }

    // 1. Find the campaign FIRST — the entrant's contact belongs to the campaign's owner, and the
    //    `contact_tenants` link the upsert writes below is what makes the lead visible to that
    //    owner (kanban t_369cb159).
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    // 2. Upsert contact (shared identity row) and link it to the campaign's account.
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        website: body.contact.website.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(
        &state.db,
        &contact_input,
        Some(campaign.account_id),
        "raffle",
    )
    .await?;

    // Play-time gate: enforce the campaign owner's plan tier includes this mechanic
    // (402 before any entry is created for free/subtracted tiers).
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    // 3. Enter raffle
    let entry_id = raffles::enter_raffle(&state.db, &campaign.id, &contact_id).await?;

    // 4. Campaign referral loop (kanban t_6723eb30).
    //
    // The play page has always sent `ref` on the raffle body; this handler used to ignore it, so a
    // raffle participant got no share link and a friend arriving through one was never credited.
    // Same two directions the generic `/api/v1/entries` path wires (t_ad98b6ab):
    //   * an entry that arrived with `?ref=<code>` credits the referrer of that code;
    //   * every entrant also gets their OWN code + share link for this campaign, which the play
    //     page renders as the share box from the response below.
    // Both are best-effort: the referral is a reward on top of the entry, so a referral write must
    // never turn a valid raffle entry into a 500. A failure is logged, never swallowed silently.
    if let Some(ref_code) = body.referral_code.as_deref().filter(|c| !c.is_empty()) {
        if let Err(e) = crate::handlers::viral_handler::handle_referral_credit(
            &state,
            &campaign.id,
            Some(ref_code),
            &contact_id,
            "raffle",
            &campaign.config,
        )
        .await
        {
            tracing::warn!(
                "raffle entry {entry_id}: referral credit for code {ref_code:?} failed: {e}"
            );
        }
    }
    let (referral_code, referral_link) = match crate::db::viral::ensure_campaign_referral(
        &state.db,
        &campaign.id,
        &contact_id,
        "participant",
    )
    .await
    {
        Ok(r) => (
            Some(r.referral_code.clone()),
            Some(format!(
                "{}/c/{}?ref={}",
                crate::email::APP_URL,
                campaign.slug,
                r.referral_code
            )),
        ),
        Err(e) => {
            tracing::warn!(
                "raffle entry {entry_id}: could not mint a campaign referral code for campaign {}: {e}",
                campaign.slug
            );
            (None, None)
        }
    };

    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "message": "Successfully entered raffle",
        "referral_code": referral_code,
        "referral_link": referral_link
    })))
}

/// POST /api/v1/raffles/:slug/draw — authenticated, seeded Fisher-Yates.
///
/// SECURITY (kanban t_9951bc00): the campaign is resolved under the CALLER's account, not by slug
/// alone. This handler took an `AuthenticatedUser` and then threw it away, so any signed-in tenant
/// could draw a winner for another tenant's raffle. A foreign or absent slug is a 404 (the app's
/// convention — never 403), matching `campaign_for_caller` in every other campaign-authoring route.
pub async fn draw(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    // Find campaign
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    // Play-time gate: enforce the campaign owner's plan tier includes this mechanic
    // (402 before any draw is run for free/subtracted tiers).
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    // Check if already drawn
    if raffles::has_existing_draw(&state.db, &campaign.id).await? {
        return Err(AppError::BadRequest(
            "This raffle has already been drawn. Use redraw instead.".to_string(),
        ));
    }

    // Get all entries
    let entry_ids = raffles::get_entries_for_draw(&state.db, &campaign.id).await?;
    if entry_ids.is_empty() {
        return Err(AppError::BadRequest("No entries to draw from".to_string()));
    }

    // Generate random seed
    use rand::Rng;
    let seed: u64 = rand::thread_rng().gen();

    // Convert entry IDs to strings
    let entry_strings: Vec<String> = entry_ids.iter().map(|id| id.to_string()).collect();

    // Perform seeded shuffle
    let winner_id = raffle_draw::seeded_fisher_yates(&entry_strings, seed)
        .ok_or_else(|| AppError::Internal("No winner found".to_string()))?;

    let winner_uuid = uuid::Uuid::parse_str(&winner_id)
        .map_err(|_| AppError::Internal("Invalid winner UUID".to_string()))?;

    // Record draw
    raffles::record_draw(&state.db, &campaign.id, &winner_uuid, seed).await?;

    Ok(Json(json!({
        "winner_entry_id": winner_id,
        "seed": seed,
        "total_entries": entry_ids.len(),
    })))
}

/// POST /api/v1/raffles/:slug/redraw — authenticated, must already have a seed.
///
/// SECURITY (kanban t_9951bc00): same defect and same fix as `draw` above — the campaign comes from
/// the caller's own account, so a foreign slug is a 404 instead of a re-draw of another tenant's
/// raffle.
pub async fn redraw(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    // Find campaign
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    // Play-time gate: enforce the campaign owner's plan tier includes this mechanic
    // (402 before any redraw is run for free/subtracted tiers).
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    // Get all entries (excluding current winner)
    let mut entry_ids = raffles::get_entries_for_draw(&state.db, &campaign.id).await?;

    // Remove the current winner from the pool
    let current_winner_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM entries WHERE campaign_id = $1 AND outcome = 'winner' LIMIT 1",
    )
    .bind(campaign.id)
    .fetch_optional(&state.db)
    .await?;

    if let Some(ref winner) = current_winner_id {
        entry_ids.retain(|id| id != winner);
    }

    if entry_ids.is_empty() {
        return Err(AppError::BadRequest(
            "No remaining entries to redraw from".to_string(),
        ));
    }

    // Get existing seed. `config->>'draw_seed'` is TEXT (the value is written as
    // `to_jsonb(seed::text)`), so it must NOT be decoded as `serde_json::Value`: that raised
    // `mismatched types ... JSONB ... TEXT` and made this whole arm answer 500 for every caller,
    // owner included (kanban t_9951bc00).
    let seed_value: Option<String> =
        sqlx::query_scalar("SELECT config->>'draw_seed' FROM campaigns WHERE id = $1")
            .bind(campaign.id)
            .fetch_optional(&state.db)
            .await?
            .flatten();

    let seed: u64 = match seed_value {
        Some(s) => s.parse().unwrap_or(0),
        None => {
            return Err(AppError::BadRequest(
                "No existing draw seed found. Must draw first.".to_string(),
            ))
        }
    };

    // Use a variant of the seed for redraw
    let redraw_seed = seed.wrapping_add(1);

    // Perform seeded shuffle
    let entry_strings: Vec<String> = entry_ids.iter().map(|id| id.to_string()).collect();
    let new_winner_id = raffle_draw::seeded_fisher_yates(&entry_strings, redraw_seed)
        .ok_or_else(|| AppError::Internal("No winner found".to_string()))?;

    let new_winner_uuid = uuid::Uuid::parse_str(&new_winner_id)
        .map_err(|_| AppError::Internal("Invalid winner UUID".to_string()))?;

    // Record new draw with the redraw seed
    raffles::record_draw(&state.db, &campaign.id, &new_winner_uuid, redraw_seed).await?;

    Ok(Json(json!({
        "winner_entry_id": new_winner_id,
        "seed": redraw_seed,
        "total_entries": entry_ids.len(),
    })))
}
