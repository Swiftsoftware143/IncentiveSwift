//! Quiz/Trivia handler — question CRUD, quiz submission, scoring, CRM field mapping.

use crate::db::{campaigns, contacts, questions_answers};
use crate::error::AppError;
use crate::handlers::campaigns::campaign_for_caller;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/campaigns/{slug}/questions — authoring view (includes `correct_answer`).
///
/// SECURITY (kanban t_734f1f94): this route had NO `AuthenticatedUser` extractor and resolved the
/// campaign by slug alone, so it answered ANONYMOUSLY (measured live: `GET` with no credential ->
/// 200) and, for a tenant, about any account's campaign. Scoped now; the player's own feed is the
/// public `/api/v1/play/{campaign_id}/questions`, which never carries `correct_answer`.
pub async fn list_campaign_questions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    let questions = questions_answers::get_campaign_questions(&state.db, &campaign.id).await?;
    Ok(Json(
        json!({ "questions": questions, "campaign_id": campaign.id }),
    ))
}

/// GET /api/v1/play/{campaign_id}/questions — public view (no correct_answer)
pub async fn play_campaign_questions(
    State(state): State<AppState>,
    Path(campaign_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // Play-time gate: loading the quiz questions is the first step of play, so gate
    // on the campaign owner's tier (402 before questions are served when subtracted).
    let campaign = campaigns::get_campaign_by_id(&state.db, &campaign_id).await?;
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    let questions =
        questions_answers::get_campaign_questions_public(&state.db, &campaign_id).await?;
    Ok(Json(json!({ "questions": questions })))
}

/// POST /api/v1/campaigns/{slug}/questions — create a question
pub async fn create_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(input): Json<questions_answers::CreateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    let id = questions_answers::create_question(&state.db, &campaign.id, &input).await?;
    Ok(Json(
        json!({ "id": id, "question_key": input.question_key }),
    ))
}

/// PUT /api/v1/campaigns/{slug}/questions/{question_id} — update a question
///
/// SECURITY (kanban t_27e3e083): the campaign resolved under the caller's account AND the question
/// id are BOTH part of the write. Resolving only the slug left the id free, so a tenant could name
/// its own campaign and a question id belonging to another tenant's campaign and rewrite that row.
/// No row matched -> 404, the house refusal for a foreign or absent id.
pub async fn update_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, question_id)): Path<(String, Uuid)>,
    Json(input): Json<questions_answers::UpdateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    let updated =
        questions_answers::update_question(&state.db, &campaign.id, &question_id, &input).await?;
    if !updated {
        return Err(AppError::NotFound("Question not found".to_string()));
    }
    Ok(Json(json!({ "status": "updated" })))
}

/// DELETE /api/v1/campaigns/{slug}/questions/{question_id}
///
/// SECURITY (kanban t_27e3e083): the same predicate as `update_question` — the DELETE binds the
/// campaign resolved under the caller's account, so a foreign question id deletes nothing and the
/// route answers 404.
pub async fn delete_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, question_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    let deleted = questions_answers::delete_question(&state.db, &campaign.id, &question_id).await?;
    if !deleted {
        return Err(AppError::NotFound("Question not found".to_string()));
    }
    Ok(Json(json!({ "status": "deleted" })))
}

/// Input for quiz submission
#[derive(Debug, Deserialize)]
pub struct QuizSubmitInput {
    pub contact: QuizContact,
    pub answers: Vec<QuizAnswer>,
    pub source: Option<QuizSource>,
    /// The referral code this visitor arrived with (`?ref=` on a shared campaign link). The served
    /// play page forwards it so the referrer is credited on the QUIZ submit — the same two
    /// directions `POST /api/v1/entries` wires (kanban t_ad98b6ab); before t_6723eb30 a quiz
    /// participant got no share link and a friend arriving through one was never credited.
    #[serde(rename = "ref", default)]
    pub referral_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuizContact {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub company: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuizAnswer {
    pub question_id: Uuid,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct QuizSource {
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Response from quiz submission
#[derive(Debug, Serialize)]
pub struct QuizResult {
    pub score: i32,
    pub max_score: i32,
    pub percentage: f64,
    pub passed: bool,
    pub persona: String,
    pub persona_tag: String,
    pub entry_id: Uuid,
    pub crm_fields: Value,
    /// The hub's post-outcome redirect, when the campaign's `delivery_config` asks for
    /// one. The spin path returns the same field; absent when no redirect is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// This participant's own campaign referral share link (kanban t_6723eb30) — the same
    /// producer the generic entry path returns, so the served play page renders the share box
    /// from this response. Null only when minting failed.
    pub referral_code: Option<String>,
    pub referral_link: Option<String>,
}

/// POST /api/v1/quiz/{campaign_id}/submit — submit quiz answers, score, create entry
pub async fn submit_quiz(
    State(state): State<AppState>,
    Path(campaign_id): Path<Uuid>,
    Json(input): Json<QuizSubmitInput>,
) -> Result<Json<Value>, AppError> {
    // Get campaign
    let campaign = campaigns::get_campaign_by_id(&state.db, &campaign_id).await?;

    // Play-time gate: enforce the campaign owner's plan tier includes this mechanic
    // (402 before any quiz is scored/recorded for free/subtracted tiers).
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    let passing_score = campaign
        .config
        .get("passing_score")
        .and_then(|v| v.as_f64())
        .unwrap_or(70.0);

    // Score the answers
    let answer_inputs: Vec<questions_answers::AnswerInput> = input
        .answers
        .iter()
        .map(|a| questions_answers::AnswerInput {
            question_id: a.question_id,
            value: a.value.clone(),
            raw_value: None,
        })
        .collect();

    let (score, max_score, percentage) =
        questions_answers::score_quiz_submission(&state.db, &campaign.id, &answer_inputs).await?;

    let passed = percentage >= passing_score;

    // Determine persona from outcome_tags
    let (persona, persona_tag) =
        questions_answers::determine_persona(percentage, &campaign.outcome_tags);

    // Build CRM fields from question mappings
    let questions = questions_answers::get_campaign_questions(&state.db, &campaign.id).await?;
    let mut crm_fields = json!({
        "quiz_score": score,
        "quiz_max_score": max_score,
        "quiz_percentage": percentage,
        "persona": persona,
        "persona_tag": persona_tag,
        "passed": passed,
    });

    // Map answer values to CRM fields where configured
    for answer in &input.answers {
        if let Some(q) = questions.iter().find(|q| q.id == answer.question_id) {
            if let Some(ref crm_field) = q.crm_field {
                if let Some(ref crm_type) = q.crm_field_type {
                    let key = format!("{}_{}", crm_type, crm_field);
                    crm_fields[key] = json!(answer.value);
                }
            }
        }
    }

    // Add source/UTM
    if let Some(ref source) = input.source {
        if let Some(ref v) = source.utm_source {
            crm_fields["utm_source"] = json!(v);
        }
        if let Some(ref v) = source.utm_medium {
            crm_fields["utm_medium"] = json!(v);
        }
        if let Some(ref v) = source.utm_campaign {
            crm_fields["utm_campaign"] = json!(v);
        }
        if let Some(ref v) = source.referrer_url {
            crm_fields["referrer_url"] = json!(v);
        }
    }

    // Create/upsert contact. This used to be a hand-rolled
    // `ON CONFLICT (email) WHERE email IS NOT NULL AND email <> ''`, which matches NO
    // index that exists: the live schema dedups on
    // `contacts_email_idx (lower(email)) WHERE email IS NOT NULL`, so every submission
    // died on the ON CONFLICT inference before an entry — or the integration hub — was
    // ever reached. Use the crate's canonical upsert, the same one the play paths use.
    let contact_id = contacts::upsert_contact(
        &state.db,
        &contacts::ContactInput {
            first_name: input.contact.first_name.clone(),
            last_name: input.contact.last_name.clone(),
            email: Some(input.contact.email.clone()),
            phone: input.contact.phone.clone(),
            business_name: input.contact.company.clone(),
            website: None,
        },
        Some(campaign.account_id),
        "quiz",
    )
    .await
    .map_err(|e| AppError::Database(format!("Contact upsert failed: {}", e)))?;

    // The campaign's own daily entry cap (`config.max_spins_per_day`) — the same guard
    // `handlers::entries` applies to every mechanic it captures. A served quiz used to reach
    // this campaign through POST /api/v1/entries, so the cap applied; the mechanic's own submit
    // path must not become the way around it (kanban t_d8eef6ae).
    crate::mechanics::pity_timer::check_daily_limit(
        &state.db,
        &campaign.id,
        &contact_id,
        &campaign.config,
    )
    .await?;

    // Create entry (entries table has no account_id column, only contact_id + campaign_id)
    // The account that owns this campaign has to be under its plan's lead allowance.
    crate::features::enforce_lead_limit_for_campaign(&state.db, campaign.id).await?;
    let entry_id = Uuid::new_v4();
    // The tag this submission applied: the quiz's own persona tag when the campaign's
    // `outcome_tags` names one, else the `<namespace>_entrant` tag the generic entry path would
    // have written. EVERY entry-producing handler fills this column — it is what the output
    // actions, the CoreSwift push and the webhook payload carry — so a quiz entry must not be
    // the one kind that arrives untagged.
    let quiz_tag = if persona_tag.trim().is_empty() {
        format!("{}_entrant", campaign.tag_namespace)
    } else {
        persona_tag.clone()
    };
    // The entry's own `answers` value, built once: it is what the row stores, and the same
    // object is what the lifecycle sender reads its prize fields from.
    let entry_answers = json!({
        "persona": persona.clone(),
        "persona_tag": persona_tag.clone(),
        "crm_fields": crm_fields.clone()
    });
    sqlx::query(
        r#"INSERT INTO entries (id, campaign_id, contact_id, answers, score, outcome, tags_applied,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
    )
    .bind(entry_id)
    .bind(campaign.id)
    .bind(contact_id)
    .bind(entry_answers.clone())
    .bind(score)
    .bind(if passed { "won" } else { "lost" })
    .bind(vec![quiz_tag.clone()])
    .bind(input.source.as_ref().and_then(|s| s.utm_source.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.utm_medium.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.utm_campaign.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.referrer_url.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.page_url.as_ref()))
    .execute(&state.db)
    .await?;

    // Store individual answers
    for answer in &input.answers {
        questions_answers::create_answer(
            &state.db,
            &entry_id,
            &answer.question_id,
            &answer.value,
            None,
        )
        .await?;
    }

    // Fire delivery integration with clean CRM payload (only crm_fields, not raw answers).
    // The config is the campaign's own `delivery_config` jsonb: this call site used to hand
    // the hub `DeliveryConfig::default()`, which is why the hub's four arms (email, webhook
    // targets, redirect, autoresponder) could never fire from ANY route. The column speaks
    // two vocabularies — see `DeliveryConfig::from_campaign_json`.
    use crate::delivery::integration_hub::{
        self, CampaignInfo, ContactInfo, DeliveryConfig, DeliveryContext, OutcomePayload,
    };
    let delivery_config = DeliveryConfig::from_campaign_json(&campaign.delivery_config);
    let delivery_config_empty = delivery_config.is_empty();
    let delivery = integration_hub::execute_delivery(
        &state.db,
        &DeliveryContext {
            // The entry this submission just created (:229). Without it the hub
            // invented a uuid and every delivery_log insert failed its FK.
            entry_id,
            campaign: CampaignInfo {
                id: campaign.id,
                name: campaign.name.clone(),
                slug: campaign.slug.clone(),
                account_id: campaign.account_id,
            },
            contact: ContactInfo {
                id: contact_id,
                email: Some(input.contact.email.clone()),
                phone: input.contact.phone.clone(),
                first_name: input.contact.first_name.clone(),
                last_name: input.contact.last_name.clone(),
            },
            outcome: OutcomePayload {
                prize_id: None,
                prize_label: if passed { Some(persona.clone()) } else { None },
                prize_type: Some("quiz".to_string()),
                won: passed,
                was_pity: false,
                streak: 0,
                total_spins: 0,
                redemption_url: None,
            },
            delivery_config,
            crm_fields: Some(crm_fields.clone()),
        },
    )
    .await;

    // The hub's result used to be thrown away here (`let _ =`), so a tenant whose config
    // asks for a win email, a webhook target or the autoresponder had no way to tell that
    // nothing ran. One line, with the numbers that decide it.
    tracing::info!(
        "quiz delivery for entry {}: config_empty={} email_sent={} redirect={:?} webhooks_fired={} autoresponder_fired={} errors={:?}",
        entry_id,
        delivery_config_empty,
        delivery.email_sent,
        delivery.redirect_url,
        delivery.webhooks_fired.len(),
        delivery.autoresponder_fired,
        delivery.errors,
    );

    // ---- The rest of what an entry on this campaign MEANS ---------------------------------
    // A served quiz used to be captured through POST /api/v1/entries (play.html's generic arm),
    // and these are the legs that path ran: the daily record for the cap checked above, the
    // campaign's loyalty bridge, its lifecycle mail, its CoreSwift push, its output actions and
    // its direct integrations. play.html now plays through THIS handler, so they are carried
    // here — without them, switching the served page to the quiz endpoint would silently stop a
    // quiz campaign's mail, its CRM push and its automations (kanban t_d8eef6ae).
    crate::mechanics::pity_timer::record_daily_spin(&state.db, &campaign.id, &contact_id).await?;

    // Loyalty bridge — auto-enroll and award points when the campaign links a loyalty program.
    // Best-effort, exactly as on the entry path.
    if campaign.auto_enroll_loyalty {
        if let Some(program_id) = campaign.loyalty_program_id {
            let points = campaign.loyalty_points_per_play;
            let _ = crate::mechanics::loyalty_checkin::process_checkin_from_entry(
                &state,
                &program_id.to_string(),
                &contact_id.to_string(),
                &entry_id.to_string(),
                &campaign.slug,
                points,
            )
            .await;
        }
    }

    // Lifecycle email — stage 1 immediately + stage 3 queued 24h, rendered from the ONE var set
    // the platform can answer for this entry (`lifecycle_emails::entry_email_vars`). `score` is
    // the number `score_quiz_submission` computed above, which is the whole point of this card:
    // a score-shaped follow-up binds a real number instead of mailing braces. Dedupe is per
    // contact per campaign, the same rule the entry path applies.
    let to_email = input.contact.email.trim().to_string();
    if !to_email.is_empty() {
        let already = crate::lifecycle_emails::already_emailed(
            &state.db,
            campaign.account_id,
            &to_email,
            &campaign.r#type,
        )
        .await;
        if !already {
            let state_clone = state.clone();
            let campaign_clone = campaign.clone();
            let em = to_email.clone();
            let answers_clone = entry_answers.clone();
            let fname = input.contact.first_name.clone();
            let lname = input.contact.last_name.clone();
            let cid = contact_id;
            let mail_score = score;
            tokio::spawn(async move {
                let vars = crate::lifecycle_emails::entry_email_vars(
                    &state_clone,
                    &campaign_clone,
                    cid,
                    entry_id,
                    Some(mail_score),
                    Some(&answers_clone),
                    fname.as_deref(),
                    lname.as_deref(),
                    &em,
                )
                .await;
                crate::lifecycle_emails::trigger_entry_lifecycle(
                    &state_clone,
                    campaign_clone.account_id,
                    &em,
                    &campaign_clone.r#type,
                    &vars,
                )
                .await;
            });
        }
    }

    // CoreSwift external push (the tenant's own BYOK connection + campaign list): a lead
    // captured by a quiz has to reach the CRM hub exactly as a lead captured by any other
    // mechanic does. Fire-and-forget.
    {
        let state_clone = state.clone();
        let push_contact_id = contact_id;
        let push_campaign_id = campaign.id;
        let push_entry_id = entry_id;
        let push_tags: Vec<String> = vec![quiz_tag.clone()];
        tokio::spawn(async move {
            crate::delivery::coreswift_external::push_entry_to_coreswift(
                &state_clone,
                &push_contact_id,
                &push_campaign_id,
                &push_entry_id,
                &push_tags,
            )
            .await;
        });
    }

    // Output actions (webhook, CoreSwift sync, email, SMS — `config.output_actions`).
    // Fire-and-forget, as on the entry path.
    {
        let state_clone = state.clone();
        let oa_campaign_id = campaign.id;
        let oa_campaign_name = campaign.name.clone();
        let oa_campaign_slug = campaign.slug.clone();
        let oa_campaign_type = campaign.r#type.clone();
        let oa_campaign_config = campaign.config.clone();
        let oa_account_id = campaign.account_id;
        let oa_outcome = if passed { "won" } else { "lost" }.to_string();
        let oa_tags = vec![quiz_tag.clone()];
        let oa_answers = entry_answers.clone();
        let oa_contact_id = contact_id;
        let fn1 = input.contact.first_name.clone().unwrap_or_default();
        let ln1 = input.contact.last_name.clone().unwrap_or_default();
        let em1 = to_email.clone();
        let ph1 = input.contact.phone.clone().unwrap_or_default();
        let bn1 = input.contact.company.clone().unwrap_or_default();
        let utm_source = input.source.as_ref().and_then(|s| s.utm_source.clone());
        let utm_medium = input.source.as_ref().and_then(|s| s.utm_medium.clone());
        let utm_campaign = input.source.as_ref().and_then(|s| s.utm_campaign.clone());
        let referrer_url = input.source.as_ref().and_then(|s| s.referrer_url.clone());
        let page_url = input.source.as_ref().and_then(|s| s.page_url.clone());
        tokio::spawn(async move {
            crate::delivery::output_actions::execute_output_actions(
                &state_clone,
                &oa_campaign_id,
                &oa_campaign_name,
                &oa_campaign_slug,
                &oa_campaign_type,
                &oa_campaign_config,
                &oa_contact_id,
                &fn1,
                &ln1,
                &em1,
                &ph1,
                "",
                &bn1,
                &oa_account_id,
                &oa_outcome,
                &oa_tags,
                Some(score as f64),
                Some(&oa_answers),
                utm_source.as_deref(),
                utm_medium.as_deref(),
                utm_campaign.as_deref(),
                referrer_url.as_deref(),
                page_url.as_deref(),
            )
            .await;
        });
    }

    // Direct integrations (`delivery_config.integrations[]` — the second vocabulary in that
    // column, read by `handlers::entries::dispatch_integrations`), with the quiz's own question
    // text paired to the visitor's answer. Best-effort: the entry is committed, so a broken
    // tenant webhook must not swallow the score the customer came for.
    let qa_pairs: Vec<crate::delivery::payload::QuestionAnswerPair> = input
        .answers
        .iter()
        .filter_map(|a| {
            questions.iter().find(|q| q.id == a.question_id).map(|q| {
                crate::delivery::payload::QuestionAnswerPair {
                    question: q.question_text.clone(),
                    answer: a.value.clone(),
                }
            })
        })
        .collect();
    let payload = crate::delivery::payload::DeliveryPayload::build(
        crate::delivery::payload::ContactPayload {
            first_name: input.contact.first_name.clone(),
            last_name: input.contact.last_name.clone(),
            email: Some(to_email.clone()),
            phone: input.contact.phone.clone(),
            website: None,
            business_name: input.contact.company.clone(),
        },
        crate::delivery::payload::CampaignPayload {
            name: campaign.name.clone(),
            campaign_type: campaign.r#type.clone(),
            tag_namespace: campaign.tag_namespace.clone(),
        },
        if passed { "won" } else { "lost" }.to_string(),
        vec![quiz_tag.clone()],
        Some(score),
        qa_pairs,
        entry_id.to_string(),
    );
    if let Err(e) = crate::handlers::entries::dispatch_integrations(
        &state.http_client,
        &campaign.delivery_config,
        &payload,
        &state.db,
        &entry_id,
        &campaign.account_id,
    )
    .await
    {
        tracing::warn!("quiz direct integrations for entry {}: {e}", entry_id);
    }

    // Campaign referral loop (kanban t_6723eb30).
    //
    // `POST /api/v1/entries` wires both directions (t_ad98b6ab); the quiz's own submit path skipped
    // them entirely, so a quiz participant got no share link and a friend arriving through one was
    // never credited. Same two directions, same helpers:
    //   * a submit that arrived with `?ref=<code>` credits the referrer of that code;
    //   * every quiz participant also gets their OWN code + share link for this campaign, which the
    //     play page renders as the share box from the response below.
    // Both are best-effort: the referral is a reward on top of the scored entry, so a referral write
    // must never turn a valid submission into a 500. A failure is logged, never swallowed silently.
    if let Some(ref_code) = input.referral_code.as_deref().filter(|c| !c.is_empty()) {
        if let Err(e) = crate::handlers::viral_handler::handle_referral_credit(
            &state,
            &campaign.id,
            Some(ref_code),
            &contact_id,
            "quiz",
            &campaign.config,
        )
        .await
        {
            tracing::warn!(
                "quiz entry {entry_id}: referral credit for code {ref_code:?} failed: {e}"
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
                "quiz entry {entry_id}: could not mint a campaign referral code for campaign {}: {e}",
                campaign.slug
            );
            (None, None)
        }
    };

    let result = QuizResult {
        score,
        max_score,
        percentage: (percentage * 100.0).round() / 100.0,
        passed,
        persona,
        persona_tag,
        entry_id,
        crm_fields,
        redirect_url: delivery.redirect_url.clone(),
        referral_code,
        referral_link,
    };

    Ok(Json(json!(result)))
}
