//! Lifecycle email triggers — fires the correct entry/result/follow-up template
//! for each campaign type using the email_queue (scheduler) + sender.
//!
//! ## Where the merge-field values come from (kanban t_375c8c40)
//!
//! The stage-1 (entry) and stage-3 (24h follow-up) mails used to be rendered from exactly
//! `{first_name, last_name, email, campaign_name, campaign_type}`. The shipped
//! `email_templates` rows, however, ask for more than that — `{{user_score}}`,
//! `{{ticket_number}}`, `{{prize_name}}`, `{{reward_code}}`, `{{share_link}}`,
//! `{{referral_link}}` — so those mails left the box with the braces in them (and, since
//! `template_render::warn_unsubstituted`, with a warn naming each one).
//!
//! `entry_email_vars` is the ONE place that now answers "what does the platform know about
//! THIS entry", and it answers from the database, never by guessing:
//!
//! | key             | source                                                                     |
//! |-----------------|----------------------------------------------------------------------------|
//! | `ticket_number` | the entry's own reference (`entries.id`, first 8 hex, uppercased) — the platform has no OTHER ticket number |
//! | `user_score`    | the score the CALLER supplied (`entries.score`); never invented             |
//! | `prize_name`    | the contact's own `campaign_wins.prize_label` in this campaign -> the entry's `answers` (`prize_label` / `reward.label`) -> `campaigns.config->>'prize_name'` |
//! | `reward_code`   | the same win's `redemption_code` -> the entry's `answers->>'redemption_code'` |
//! | `share_link` / `referral_link` | the campaign's public play URL — the same link the console's own "Direct Link" shows (`{origin}/play/{slug}`) |
//!
//! A key whose value does not exist is **not inserted**, so the placeholder stays in the
//! body and `template_render::warn_unsubstituted` names it: a value the platform cannot
//! answer is loud, never silently blank.

use crate::db::campaigns::Campaign;
use crate::delivery::sender;
use crate::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// Every campaign type this sender fires for, and the two `email_templates` rows it fires
/// (`entry` immediately, `followup` 24h later). Stage 2 (result) is the winner path in
/// `handlers::entries` step 8, which resolves `{campaign_type}_winner` and falls back to the
/// literal `winner`.
///
/// THE RULE THIS TABLE ENFORCES (kanban t_1adeb952): a key here MUST be a member of
/// `db::campaigns::VALID_MECHANIC_TYPES`, because that is the only vocabulary a campaign can
/// carry — `POST /api/v1/campaigns` refuses anything else (`db::campaigns::create_campaign` ->
/// `validate_mechanic_type`) and the served console's MECHANICS list is those same 15 strings.
/// The shipped map keyed five arms on strings no campaign can ever hold — `survey`, `iqs`,
/// `secret_codes`, `tier` and `scratch` — so the ten rows behind them could not be selected by
/// anything (measured: `/opt/swift/audits/t_1adeb952/20-census.txt`).
///
///   * `scratch` was the one drifted key whose mechanic EXISTS under another name
///     (`scratch_card`), so the arm is corrected here: a scratch-card campaign now fires
///     `scratch_confirm_prize` / `second_chance_replay` instead of falling to the default arm.
///   * the other four arms are DELETED, and the rows they named are retired by
///     `20260927_retire_unreachable_email_templates.sql` — the map no longer claims a producer
///     for a mechanic the product cannot create.
///
/// `every_arm_keys_a_creatable_mechanic` keeps the next drift from landing.
const LIFECYCLE_MAP: &[(&str, &str, &str)] = &[
    ("quiz", "entry_ack", "challenge_share"),
    ("poll", "vote_confirm", "next_topic"),
    ("spin_wheel", "win_voucher", "post_redemption_thanks"),
    ("raffle", "entry_ticket", "bonus_entry_prompt"),
    ("calculator", "calc_summary", "re_run_prompt"),
    ("b2b_loyalty", "welcome_listing", "loyalty_digest"),
    ("mystery", "mystery_secured", "urgent_expiry_notice"),
    ("countdown", "registration_lockin", "post_deadline_followup"),
    ("score_reveal", "processing_notice", "improvement_roadmap"),
    (
        "scratch_card",
        "scratch_confirm_prize",
        "second_chance_replay",
    ),
    (
        "long_form_qualifier",
        "application_received",
        "review_complete_decision",
    ),
];

/// What a creatable campaign type with no arm of its own fires — `personality`, `chat`,
/// `leaderboard` and `loyalty` reach the sender through this pair.
const DEFAULT_LIFECYCLE: (&str, &str) = ("entry_confirmation", "challenge_share");

/// Map a campaign type → (entry_template, followup_template).
/// Stage 2 (result) is handled inline in create_entry (winner/result path).
pub fn lifecycle_templates(campaign_type: &str) -> (&'static str, &'static str) {
    for (key, entry, followup) in LIFECYCLE_MAP {
        if *key == campaign_type {
            return (entry, followup);
        }
    }
    DEFAULT_LIFECYCLE
}

/// The entry's ticket reference: the first 8 hex characters of the entry id, uppercased.
/// `entries` has no ticket column and no sequence, so the entry's own id IS the ticket the
/// platform can point at — deterministic, stable, and quoteable by the recipient.
fn ticket_ref(entry_id: Uuid) -> String {
    entry_id
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>()
        .to_uppercase()
}

/// First non-empty string among `keys` in the entry's own `answers` object.
fn answer_str(answers: Option<&Value>, keys: &[&str]) -> String {
    let Some(obj) = answers.and_then(|a| a.as_object()) else {
        return String::new();
    };
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
        // the mystery handler stores `{"reward": {"label": "..."}}`
        if let Some(v) = obj
            .get(*k)
            .and_then(|v| v.get("label"))
            .and_then(|v| v.as_str())
        {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// `campaigns.config` fallbacks for a prize label.
fn config_prize_label(campaign: &Campaign) -> String {
    for ptr in ["/prize_name", "/reward/label", "/prize_label"] {
        if let Some(v) = campaign.config.pointer(ptr).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Prize label + reward code for this contact in this campaign, best-effort.
///
/// The win row is the exact artifact the app writes when a mechanic awards something
/// (`campaign_wins.prize_label`/`redemption_code`, written by `prize_draw::record_win`,
/// `scratch_handler` and `milestone_engine`), so it is consulted first; the entry's own
/// `answers` and the campaign config are the fallbacks. Anything still unknown is left out
/// of `vars` on purpose.
async fn resolve_prize_fields(
    state: &AppState,
    campaign: &Campaign,
    contact_id: Uuid,
    answers: Option<&Value>,
    vars: &mut serde_json::Map<String, Value>,
) {
    let mut label = String::new();
    let mut code = String::new();

    let win: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT prize_label, redemption_code FROM campaign_wins
         WHERE campaign_id = $1 AND contact_id = $2
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(campaign.id)
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if let Some((l, c)) = win {
        label = l.trim().to_string();
        code = c.unwrap_or_default().trim().to_string();
    }

    if label.is_empty() {
        label = answer_str(answers, &["prize_label", "prize_name", "reward"]);
    }
    if label.is_empty() {
        label = config_prize_label(campaign);
    }
    if code.is_empty() {
        code = answer_str(answers, &["redemption_code", "reward_code", "code"]);
    }

    if !label.is_empty() {
        vars.insert("prize_name".to_string(), json!(label));
    }
    if !code.is_empty() {
        vars.insert("reward_code".to_string(), json!(code));
    }
}

/// Every merge field the lifecycle sender can answer for THIS entry.
///
/// Built ONCE at the entry call site and then used for both stages: the immediate send and
/// the vars persisted in `pending_emails` (which the 24h ticker replays through the same
/// `sender::send_template_by_type`), so the two stages can never disagree.
#[allow(clippy::too_many_arguments)]
pub async fn entry_email_vars(
    state: &AppState,
    campaign: &Campaign,
    contact_id: Uuid,
    entry_id: Uuid,
    score: Option<i32>,
    answers: Option<&Value>,
    first_name: Option<&str>,
    last_name: Option<&str>,
    to_email: &str,
) -> Value {
    let mut vars = serde_json::Map::new();
    vars.insert("first_name".to_string(), json!(first_name.unwrap_or("")));
    vars.insert("last_name".to_string(), json!(last_name.unwrap_or("")));
    vars.insert("email".to_string(), json!(to_email));
    vars.insert("campaign_name".to_string(), json!(campaign.name.clone()));
    vars.insert("campaign_type".to_string(), json!(campaign.r#type.clone()));
    // The entry's own ticket reference — the only ticket number this platform has.
    vars.insert("ticket_number".to_string(), json!(ticket_ref(entry_id)));
    // The score is the CALLER's number (entries.score has no server-side producer for the
    // generic entry route). Absent = left out, so the warn names it instead of a blank.
    if let Some(s) = score {
        vars.insert("user_score".to_string(), json!(s));
    }
    // Share / referral link: the campaign's public play URL — the same string the console's
    // own "Direct Link" (GET /api/v1/embed/campaign/:slug -> play_url) hands a tenant.
    let link = format!("{}/play/{}", crate::email::APP_URL, campaign.slug);
    vars.insert("share_link".to_string(), json!(link.clone()));
    vars.insert("referral_link".to_string(), json!(link));

    resolve_prize_fields(state, campaign, contact_id, answers, &mut vars).await;

    Value::Object(vars)
}

/// Fire the stage-1 entry email immediately + schedule stage-3 follow-up (24h).
/// Best-effort: never fails the entry. `vars` comes from `entry_email_vars`.
pub async fn trigger_entry_lifecycle(
    state: &AppState,
    account_id: Uuid,
    to_email: &str,
    campaign_type: &str,
    vars: &Value,
) {
    if to_email.is_empty() {
        return;
    }
    let (entry_tpl, followup_tpl) = lifecycle_templates(campaign_type);

    // Stage 1 — immediate
    let r = sender::send_template_by_type(&state.db, account_id, to_email, entry_tpl, vars).await;
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
        vars,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this table was flattened for (kanban t_1adeb952): an arm keyed on a string no
    /// campaign can carry never matches, so the rows it names are unreachable forever while the
    /// map still claims a producer exists. Four such arms shipped (`survey`, `iqs`,
    /// `secret_codes`, `tier`) and one more was a typo (`scratch` for `scratch_card`).
    #[test]
    fn every_arm_keys_a_creatable_mechanic() {
        for (key, entry, followup) in LIFECYCLE_MAP {
            assert!(
                crate::db::campaigns::validate_mechanic_type(key),
                "lifecycle arm '{key}' is not in VALID_MECHANIC_TYPES, so no campaign can ever \
                 carry it and its rows ({entry}/{followup}) can never be selected"
            );
        }
    }

    /// The one drifted key whose mechanic exists: a scratch-card campaign must fire ITS rows,
    /// not fall through to the default pair.
    #[test]
    fn scratch_card_fires_its_own_rows() {
        assert_eq!(
            lifecycle_templates("scratch_card"),
            ("scratch_confirm_prize", "second_chance_replay")
        );
    }

    /// A creatable mechanic with no arm of its own still gets a mail (the default pair).
    #[test]
    fn an_armless_creatable_mechanic_gets_the_default_pair() {
        assert_eq!(lifecycle_templates("personality"), DEFAULT_LIFECYCLE);
        assert_eq!(lifecycle_templates("loyalty"), DEFAULT_LIFECYCLE);
    }
}
