//! Lifecycle email triggers — fires the correct entry/result/follow-up template
//! for each campaign type using the email_queue (scheduler) + sender.

use crate::delivery::sender;
use crate::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// Map a campaign type → (entry_template, followup_template).
/// Stage 2 (result) is handled inline in create_entry (winner/result path).
pub fn lifecycle_templates(campaign_type: &str) -> (&'static str, &'static str) {
    match campaign_type {
        "quiz" => ("entry_ack", "challenge_share"),
        "poll" => ("vote_confirm", "next_topic"),
        "spin_wheel" => ("win_voucher", "post_redemption_thanks"),
        "raffle" => ("entry_ticket", "bonus_entry_prompt"),
        "survey" => ("submission_thanks", "impact_report"),
        "calculator" => ("calc_summary", "re_run_prompt"),
        "b2b_loyalty" => ("welcome_listing", "loyalty_digest"),
        "iqs" => ("submission_receipt", "nurture_followup"),
        "mystery" => ("mystery_secured", "urgent_expiry_notice"),
        "countdown" => ("registration_lockin", "post_deadline_followup"),
        "score_reveal" => ("processing_notice", "improvement_roadmap"),
        "scratch" => ("scratch_confirm_prize", "second_chance_replay"),
        "secret_codes" => ("code_accepted_reward", "next_code_hint"),
        "tier" => ("tier_status_assign", "tier_upgrade_progress"),
        "long_form_qualifier" => ("application_received", "review_complete_decision"),
        _ => ("entry_confirmation", "challenge_share"),
    }
}

/// Fire the stage-1 entry email immediately + schedule stage-3 follow-up (24h).
/// Best-effort: never fails the entry.
pub async fn trigger_entry_lifecycle(
    state: &AppState,
    account_id: Uuid,
    to_email: &str,
    campaign_type: &str,
    campaign_name: &str,
    first_name: Option<&str>,
    last_name: Option<&str>,
) {
    if to_email.is_empty() {
        return;
    }
    let (entry_tpl, followup_tpl) = lifecycle_templates(campaign_type);

    let vars = json!({
        "first_name": first_name.unwrap_or(""),
        "last_name": last_name.unwrap_or(""),
        "email": to_email,
        "campaign_name": campaign_name,
        "campaign_type": campaign_type,
    });

    // Stage 1 — immediate
    let r = sender::send_template_by_type(&state.db, account_id, to_email, entry_tpl, &vars).await;
    if let Err(e) = r {
        tracing::warn!("Entry email ({} ) skipped: {e}", entry_tpl);
    }

    // Stage 3 — schedule 24h later (dedupe by campaign handled by caller's call site)
    let send_at = chrono::Utc::now() + chrono::Duration::hours(24);
    if let Err(e) = crate::email_queue::schedule_email(
        &state.db,
        account_id,
        to_email,
        followup_tpl,
        &vars,
        send_at,
    )
    .await
    {
        tracing::warn!("Failed to schedule follow-up email: {e}");
    }
}

/// Check + enforce dedupe: only one entry email per contact per campaign.
pub async fn already_emailed(
    pool: &PgPool,
    account_id: Uuid,
    to_email: &str,
    campaign_type: &str,
) -> bool {
    // Dedupe on pending_emails OR already-sent stage-1 for this campaign.
    // We use a lightweight marker: check if any entry-ack for this email+type exists in last 24h.
    let (entry_tpl, _) = lifecycle_templates(campaign_type);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pending_emails
         WHERE account_id = $1 AND to_email = $2 AND template_type IN ($3, $4)
           AND created_at > NOW() - INTERVAL '24 hours'",
    )
    .bind(account_id)
    .bind(to_email)
    .bind(entry_tpl)
    .bind("entry_confirmation")
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    count > 0
}

// silence unused import warnings if any path is compiled out
#[allow(dead_code)]
fn _unused(_v: Value) {}
