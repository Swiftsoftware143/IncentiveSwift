//! Scratch Card handler — seeded, deterministic prize reveal.
//!
//! The server decides win/lose deterministically via a seeded RNG (seed derived
//! from contact_id + campaign_id), so a given contact always scratches the same
//! result. The result is stored in `entries` (and `campaign_wins` on a win).

use crate::db::campaigns;
use crate::db::entries;
use crate::error::AppError;
use crate::handlers::mechanic_common::{
    extract_source, gate_mechanic, resolve_contact, MechanicContact,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Deserialize;
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use uuid::Uuid;

/// Request body for a scratch-card reveal.
#[derive(Debug, Deserialize)]
pub struct ScratchBody {
    pub contact: MechanicContact,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Deterministic 64-bit seed from two UUIDs.
fn seeded_key(contact_id: &Uuid, campaign_id: &Uuid) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contact_id.hash(&mut hasher);
    campaign_id.hash(&mut hasher);
    hasher.finish()
}

/// POST /api/v1/campaigns/:slug/scratch-card
pub async fn scratch(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ScratchBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "scratch_card" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'scratch_card'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "scratch_card").await?;

    let contact_id = resolve_contact(&state, &body.contact).await?;

    // Deterministic RNG seeded by (contact, campaign).
    let mut rng = StdRng::seed_from_u64(seeded_key(&contact_id, &campaign.id));
    let win_probability = campaign
        .config
        .get("win_probability")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);

    let prizes: Vec<Value> = campaign
        .config
        .get("prize_pool")
        .and_then(|p| p.get("prizes"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let won = rng.gen_bool(win_probability.clamp(0.0, 1.0)) && !prizes.is_empty();

    let (prize_id, prize_label, prize_type, outcome) = if won {
        let idx = rng.gen_range(0..prizes.len());
        let prize = &prizes[idx];
        let id = prize
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("prize")
            .to_string();
        let label = prize
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("Prize")
            .to_string();
        let ptype = prize
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("coupon")
            .to_string();
        (Some(id), Some(label), ptype, "winner".to_string())
    } else {
        (None, None, "none".to_string(), "loss".to_string())
    };

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "prize_id": prize_id,
                "prize_label": prize_label,
                "prize_type": prize_type,
                "seed": seeded_key(&contact_id, &campaign.id),
            }),
            score: None,
            outcome: Some(outcome.clone()),
            tags_applied: Some(vec![]),
            utm_source: body.utm_source.clone(),
            utm_medium: body.utm_medium.clone(),
            utm_campaign: body.utm_campaign.clone(),
            referrer_url: body.referrer_url.clone(),
            page_url: body.page_url.clone(),
            user_agent,
            ip_address,
        },
    )
    .await?;

    // On a win, record into campaign_wins for the existing wins/redeem surface.
    let mut redemption_code: Option<String> = None;
    if won {
        let code = format!("SCRATCH-{:06}", rng.gen_range(100000..999999));
        redemption_code = Some(code.clone());
        let _ = sqlx::query(
            r#"INSERT INTO campaign_wins (entry_id, contact_id, campaign_id, prize_id, prize_label, prize_type, redeemed, redemption_code)
               VALUES ($1, $2, $3, $4, $5, $6, false, $7)"#,
        )
        .bind(entry_id)
        .bind(contact_id)
        .bind(campaign.id)
        .bind(prize_id.clone().unwrap_or_default())
        .bind(prize_label.clone().unwrap_or_default())
        .bind(&prize_type)
        .bind(&code)
        .execute(&state.db)
        .await;
    }

    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "won": won,
        "outcome": outcome,
        "prize_id": prize_id,
        "prize_label": prize_label,
        "prize_type": prize_type,
        "redemption_code": redemption_code,
    })))
}
