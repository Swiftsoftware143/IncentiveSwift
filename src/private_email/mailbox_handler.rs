use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde_json::json;
use uuid::Uuid;

use super::feature_gate;
use super::models::*;

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

pub async fn provision_mailbox(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<ProvisionMailboxRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    feature_gate::check_mailbox_limit(&state.db, account_id).await?;

    // Get the domain
    let domain = sqlx::query_as::<_, PrivateEmailDomain>(
        "SELECT * FROM private_email_domains WHERE id = $1 AND tenant_id = $2",
    )
    .bind(req.domain_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let domain = domain.ok_or_else(|| AppError::NotFound("Domain not found".into()))?;

    // ── The readiness gate (B211, David 2026-11-11) ─────────────────────────────────────────────
    // A mailbox can only RECEIVE once the domain's mail records are live, so creating one before
    // that produces an address that silently rejects mail — the worst kind of empty promise. The
    // server refuses, and says what to press; the check is here and not only in the UI so a second
    // door (an API client) cannot skip it.
    //
    // The stored flag is not the verdict: a customer who finishes their DNS after adding the domain
    // has `verified = false` on the row while Mailgun already reports `active`. Before refusing, ask
    // the provider — and when it says active, promote the row so the whole app agrees.
    let mut domain_ready = domain.verified;
    if !domain_ready {
        if let Some(true) =
            super::domain_handler::domain_is_active(&state.db, account_id, &domain).await
        {
            if let Err(e) = sqlx::query(
                "UPDATE private_email_domains SET verified = TRUE, updated_at = NOW()
                  WHERE id = $1 AND tenant_id = $2 AND verified = FALSE",
            )
            .bind(domain.id)
            .bind(account_id)
            .execute(&state.db)
            .await
            {
                tracing::warn!(error = %e, domain = %domain.domain, "could not persist a live domain verification");
            }
            domain_ready = true;
        }
    }
    if !domain_ready {
        return Err(AppError::Validation(format!(
            "{} is not receiving mail yet, so an inbox on it would not work. Press \"Check again\" \
             on the domain once the mail records show Active, then create the inbox.",
            domain.domain
        )));
    }

    // Check for duplicate
    let email_address = format!("{}@{}", req.local_part, domain.domain);
    let existing = sqlx::query_as::<_, PrivateEmailBox>(
        "SELECT * FROM private_email_boxes WHERE tenant_id = $1 AND email_address = $2",
    )
    .bind(account_id)
    .bind(&email_address)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    if existing.is_some() {
        return Err(AppError::BadRequest("Email address already exists".into()));
    }

    // ── Mailgun domains: make sure the PER-DOMAIN inbound route exists ─────────────────────────
    // Mailgun has NO mailbox-creation API. A "mailbox" on a Mailgun domain is (a) the local
    // `private_email_boxes` row the inbox reads and (b) the domain's inbound route, which makes
    // Mailgun hand every address at that domain to this app. This arm used to POST
    // `{base}/v3/{domain}/addresses` — an endpoint Mailgun does not have — so every create on a
    // Mailgun domain answered 500 "404 page not found" (kanban t_0adbad21). Mailgun's legacy
    // `POST /v3/<domain>/mailboxes` is closed to new customers, so there is nothing to call there
    // either; the route is the only real provider-side object.
    //
    // The decision is the RECEIVER's, not the sender's (kanban t_04db9228): a Mailgun route only
    // matters when Mailgun is what delivers this domain's mail. A domain that sends through Mailgun
    // but receives through SendGrid must NOT get one — Mailgun's route quota is 5 for the whole
    // account, and an inert route would spend a slot on mail that never arrives there, while an
    // SMTP-sending domain that RECEIVES through Mailgun genuinely needs the route.
    //
    // For SendGrid receiving there is nothing to create at all: the customer points the domain's MX
    // at SendGrid and pastes our Inbound Parse URL into their own account, and this app only keeps
    // the local mailbox row.
    if domain.inbound_provider == "mailgun" {
        // A stored key this tenant's secret cannot open is a CONFIGURATION problem the caller can act
        // on (re-add the domain with a valid key). It used to surface as a bare 500 "Internal server
        // error", which named neither the cause nor the fix.
        let api_key = crate::secret_box::open(account_id, &domain.mailgun_api_key);
        if api_key.trim().is_empty() {
            return Err(AppError::Validation(
                "This domain's stored Mailgun API key cannot be read — re-add the domain with a \
                 valid key."
                    .into(),
            ));
        }

        let base_url = if domain.mailgun_region == "eu" {
            "https://api.eu.mailgun.net"
        } else {
            "https://api.mailgun.net"
        };

        // Idempotent and self-repairing: an existing route for this domain is reused, a route that
        // forwards somewhere else (the retired `coreswift.net` target) is repaired in place, and a
        // route is only created when the domain has none — so re-adding a mailbox never stacks
        // duplicate routes that would deliver each inbound message twice.
        //
        // A refusal is answered 422 with MAILGUN'S OWN SENTENCE: the operator learns what the
        // provider said instead of reading "Internal server error", which named no cause and no fix
        // (kanban t_0adbad21, same rule as t_93214280).
        ensure_mailgun_route(base_url, &api_key, &domain.domain)
            .await
            .map_err(AppError::Validation)?;
    }

    // ── The trial clock starts at the FIRST VERIFIED MAILBOX, never at signup ────────────────────
    // BYOK setup takes days of DNS, and the card's rule is explicit: 30 days from the first mailbox
    // that exists on a verified domain. A later mailbox on the same account INHERITS that window —
    // it must not restart the clock, or an account could keep a free trial alive by adding a box
    // every month. (The free plan sells one mailbox, but a plan change must not re-mint the window.)
    let window: (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT MIN(trial_started_at), MIN(trial_ends_at)
           FROM private_email_boxes
          WHERE tenant_id = $1 AND trial_started_at IS NOT NULL",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let row = sqlx::query_as::<_, PrivateEmailBox>(
        r#"
        INSERT INTO private_email_boxes
            (tenant_id, domain_id, user_id, local_part, email_address,
             access_mode, trial_started_at, trial_ends_at)
        VALUES ($1, $2, $3, $4, $5,
                CASE WHEN $6 THEN 'trial' ELSE 'paid' END,
                CASE WHEN $6 THEN COALESCE($7::timestamptz, NOW()) END,
                CASE WHEN $6 THEN COALESCE($8::timestamptz, NOW() + ($9 || ' days')::interval) END)
        RETURNING *
        "#,
    )
    .bind(account_id)
    .bind(req.domain_id)
    .bind(req.user_id)
    .bind(&req.local_part)
    .bind(&email_address)
    // A mailbox created by a plan that does NOT sell sending starts a trial window. IncentiveSwift's
    // inbox is sold as a paid allowance on the plan itself (see `feature_gate`), so there is no
    // separate sending tier and nothing to trial: every mailbox is `paid` from creation. The
    // columns are still written so the client's contract (and a future trial) has one shape.
    .bind(false)
    .bind(window.0)
    .bind(window.1)
    .bind("0")
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(serde_json::to_value(&row).unwrap()))
}

pub async fn list_mailboxes(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    let boxes = sqlx::query_as::<_, PrivateEmailBox>(
        "SELECT * FROM private_email_boxes
          WHERE tenant_id = $1 AND status = 'active'
          ORDER BY created_at DESC",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(serde_json::to_value(&boxes).unwrap()))
}

pub async fn delete_mailbox(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(box_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    let mailbox = sqlx::query_as::<_, PrivateEmailBox>(
        "SELECT * FROM private_email_boxes WHERE id = $1 AND tenant_id = $2",
    )
    .bind(box_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let mailbox = mailbox.ok_or_else(|| AppError::NotFound("Mailbox not found".into()))?;

    // A REAL delete. This used to set status='deprovisioned' and call it deleted, which is why a
    // stray mailbox (deploytest@zaarhub.com) survived the panel's own DELETE: the row was still
    // there, `list_mailboxes` still listed it, and the only way out was a hand-written SQL
    // statement. The mailbox IS the row — there is no provider-side object to leave behind (a
    // Mailgun route is per DOMAIN, not per address), so the row goes.
    //
    // The mail that already arrived through it is NOT destroyed: `private_email_messages.mailbox_id`
    // is ON DELETE SET NULL and keeps the address it arrived on, so deleting an address never
    // deletes history (B204: nothing may disappear).
    let deleted = sqlx::query("DELETE FROM private_email_boxes WHERE id = $1 AND tenant_id = $2")
        .bind(box_id)
        .bind(account_id)
        .execute(&state.db)
        .await
        .map_err(crate::error::db_error)?
        .rows_affected();

    if deleted == 0 {
        return Err(AppError::NotFound("Mailbox not found".into()));
    }

    Ok(Json(
        json!({"deleted": true, "email": mailbox.email_address}),
    ))
}

pub async fn update_mailbox(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(box_id): Path<Uuid>,
    Json(req): Json<UpdateMailboxRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    // ── WHO GETS THIS INBOX (kanban t_5f314e9a) ──────────────────────────────────────────────────
    // The person named must be an ACTIVE member of THIS workspace. The column already existed and
    // was only ever written at provision time, so "assign an inbox to a teammate" had no door: it
    // is the same object as the conversation assignment (`private_email_messages.assigned_user_id`),
    // one teammate, two things they are responsible for.
    if let Some(Some(user_id)) = req.user_id {
        let member: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM users WHERE id = $1 AND tenant_id = $2 AND is_active = true",
        )
        .bind(user_id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .map_err(crate::error::db_error)?;
        if member.is_none() {
            return Err(AppError::Validation(
                "That person is not an active member of this account.".into(),
            ));
        }
    }

    let row = sqlx::query_as::<_, PrivateEmailBox>(
        r#"
        UPDATE private_email_boxes
        SET signature = COALESCE($3, signature),
            forwarding_enabled = COALESCE($4, forwarding_enabled),
            creates_ticket = COALESCE($5, creates_ticket),
            user_id = CASE WHEN $6::bool THEN $7::uuid ELSE user_id END,
            updated_at = NOW()
        WHERE id = $1 AND tenant_id = $2
        RETURNING *
        "#,
    )
    .bind(box_id)
    .bind(account_id)
    .bind(&req.signature)
    .bind(req.forwarding_enabled)
    .bind(req.creates_ticket)
    // `mentioned` / `person`: an absent key leaves the assignment alone, an explicit null clears it,
    // a uuid sets it. Binding a bare Option<Option<Uuid>> is not a thing sqlx can carry, so the two
    // facts travel as two binds and the CASE decides.
    .bind(req.user_id.is_some())
    .bind(req.user_id.flatten())
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    match row {
        Some(mb) => Ok(Json(serde_json::to_value(&mb).unwrap())),
        None => Err(AppError::NotFound("Mailbox not found".into())),
    }
}

/// The public URL Mailgun forwards inbound mail to.
///
/// This was the literal `https://coreswift.net/api/v1/webhooks/mailgun/inbound` until kanban
/// t_0adbad21: `coreswift.net` is in NO vhost on this fleet (origin 404, edge 302 to a parked
/// page), so every route this app created forwarded inbound mail to a dead host. The app's own
/// public origin is [`crate::email::APP_URL`], and this path is listed in
/// [`crate::auth::route_policy`]'s public routes — Mailgun arrives with no session, so the
/// receiver has to be reachable anonymously.
fn inbound_webhook_url() -> String {
    format!("{}/api/v1/webhooks/mailgun/inbound", crate::email::APP_URL)
}

/// Mailgun's own words about a refused call.
///
/// Its `{"message": "..."}` when it sends one, the (trimmed, capped) body otherwise, plus the
/// status code so a 401 and a 404 stay distinguishable to the operator.
async fn mailgun_refusal(resp: reqwest::Response) -> String {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let sentence = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    if sentence.trim().is_empty() {
        format!("HTTP {}", status)
    } else {
        format!("{} (HTTP {})", sentence.trim(), status)
    }
}

/// Does a Mailgun route expression select mail for `domain`?
///
/// Mailgun expressions are `match_recipient("…regex…")`, optionally joined by `and`/`or`. The
/// shapes this fleet writes are `.*@domain`, `.*@(a|b|c)` and an exact address, so the recipient
/// patterns are read out of the expression with the escapes removed and compared by domain. A
/// regex engine is not a dependency of this crate, and pulling one in to answer this single
/// question would be the tail wagging the dog: a pattern this cannot read answers "not covered",
/// which creates a redundant route instead of silently claiming coverage that does not exist.
fn expression_selects_domain(expression: &str, domain: &str) -> bool {
    expression
        .replace("\\.", ".")
        .split(['"', '\'', '(', ')', '|', ' ', ','])
        .map(|token| token.trim().trim_end_matches('$'))
        .map(|token| token.strip_prefix(".*@").unwrap_or(token))
        .any(|token| token.eq_ignore_ascii_case(domain))
}

/// Ensure this domain has ONE inbound route forwarding to [`inbound_webhook_url`].
///
/// Routes are per DOMAIN, not per mailbox, and Mailgun's route quota is small (measured 2026-11-10:
/// 5, already full on this account), so the mailbox create must NOT add a route per mailbox:
///
/// * a route that already selects this domain and forwards to this build's receiver is REUSED —
///   which is the normal case, because the account holds one catch-all route for every CoreSwift
///   business (`match_recipient(".*@(swiftsoftware\.net|swiftimpactsolutions\.com|…")`);
/// * a route that selects this domain but forwards somewhere else (the retired `coreswift.net`
///   target) is REPAIRED in place;
/// * only a domain with no route at all gets a new one.
///
/// Every error carries Mailgun's own sentence; the caller answers it as a 4xx.
async fn ensure_mailgun_route(
    base_url: &str,
    api_key: &str,
    domain: &str,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let webhook_url = inbound_webhook_url();
    // Escaped dots: `match_recipient` takes a REGEX, so an unescaped `.` also matches
    // `you@zaarhubXcom`.
    let expression = format!(r#"match_recipient(".*@{}")"#, domain.replace('.', r"\."));
    let forward_action = format!(r#"forward("{}")"#, webhook_url);
    let description = format!("CoreSwift inbound for {}", domain);

    let list = client
        .get(format!("{}/v3/routes", base_url))
        .basic_auth("api", Some(api_key))
        .query(&[("limit", "100")])
        .send()
        .await
        .map_err(|e| format!("Mailgun route lookup failed: {}", e))?;

    if !list.status().is_success() {
        return Err(format!(
            "Mailgun refused the inbound route lookup: {}",
            mailgun_refusal(list).await
        ));
    }

    let body: serde_json::Value = list
        .json()
        .await
        .map_err(|e| format!("Mailgun route list was not JSON: {}", e))?;
    let routes: &[serde_json::Value] = body
        .get("items")
        .and_then(|items| items.as_array())
        .map(|items| items.as_slice())
        .unwrap_or(&[]);

    let selects = |route: &serde_json::Value| {
        route
            .get("expression")
            .and_then(|e| e.as_str())
            .map(|e| expression_selects_domain(e, domain))
            .unwrap_or(false)
    };
    let forwards_here = |route: &serde_json::Value| {
        route
            .get("actions")
            .and_then(|actions| actions.as_array())
            .map(|actions| {
                actions.iter().any(|a| {
                    a.as_str()
                        .map(|s| s.contains(&webhook_url))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };
    let route_id = |route: &serde_json::Value| {
        route
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::to_string)
    };

    // Covered already — including by the fleet's catch-all route. No provider write at all.
    if let Some(covered) = routes.iter().find(|r| selects(r) && forwards_here(r)) {
        return Ok(route_id(covered).unwrap_or_default());
    }

    // A route that selects this domain but forwards elsewhere is the pre-fix shape: repair it in
    // place rather than adding a competing route.
    let repair_id = routes.iter().find(|r| selects(r)).and_then(route_id);

    let form = [
        ("priority", "10"),
        ("description", description.as_str()),
        ("expression", expression.as_str()),
        ("action", forward_action.as_str()),
    ];
    let resp = match &repair_id {
        Some(route_id) => client
            .put(format!("{}/v3/routes/{}", base_url, route_id))
            .basic_auth("api", Some(api_key))
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("Mailgun route update failed: {}", e))?,
        None => client
            .post(format!("{}/v3/routes", base_url))
            .basic_auth("api", Some(api_key))
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("Mailgun route creation failed: {}", e))?,
    };

    if !resp.status().is_success() {
        let action = if repair_id.is_some() {
            "update"
        } else {
            "creation"
        };
        return Err(format!(
            "Mailgun refused the inbound route {} for {}: {}",
            action,
            domain,
            mailgun_refusal(resp).await
        ));
    }

    Ok(repair_id.unwrap_or_default())
}
