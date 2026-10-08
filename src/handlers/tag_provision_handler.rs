//! Tag provision webhook — receives FunnelSwift system tag assignments.
//!
//! There are TWO doors on this file, with deliberately DIFFERENT contracts:
//!
//! * `POST /api/v1/internal/tag-provision` (below) — the CRM CONTACT bridge. It writes one
//!   `contacts` row and nothing else, and it MUST KEEP DOING THAT (card item 4, kanban
//!   t_3724204f): it is a lead-capture door whose callers are the tags that name no account at
//!   all, so refusing or minting an account here would change a different contract. Do NOT
//!   "fix" it by making it mint accounts — the account door below exists for that.
//! * `POST /api/v1/internal/provision-free-account` (bottom) — the ACCOUNT door, which mints a
//!   real login-able account through the app's ONE signup writer (`crate::account_mint`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct TagProvisionRequest {
    pub contact: TagProvisionContact,
    pub tag: TagProvisionTag,
    pub source: String,
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct TagProvisionContact {
    pub id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub company: Option<String>,
    pub custom_fields: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct TagProvisionTag {
    pub name: String,
    pub campaign_id: Option<String>,
    pub metadata: Option<Value>,
}

pub async fn handle_tag_provision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TagProvisionRequest>,
) -> Result<impl IntoResponse, AppError> {
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = state.config.internal_sync_key.as_str();
    // An empty configured key must never authenticate a caller (kanban t_de6f2986).
    if expected.is_empty() || key != expected {
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    let email = req
        .contact
        .email
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let first_name = req
        .contact
        .first_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let last_name = req
        .contact
        .last_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let company_name = req
        .contact
        .company
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let phone = req
        .contact
        .phone
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();

    // Whose lead is this? The campaign named in the tag (contact_tenants, kanban t_369cb159).
    // FunnelSwift carries the campaign id; without it there is no derivable owner and the contact
    // stays invisible until a tenant imports or captures them (the migration's deliberate
    // direction) — a silent guess at an owner would be the leak this card removes.
    let campaign_account: Option<Uuid> = match req.tag.campaign_id.as_deref() {
        Some(cid) => match Uuid::parse_str(cid) {
            Ok(id) => sqlx::query_scalar("SELECT account_id FROM campaigns WHERE id = $1")
                .bind(id)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?,
            Err(_) => None,
        },
        None => None,
    };

    if !email.is_empty() {
        let existing: Option<(Uuid,)> =
            sqlx::query_as(r#"SELECT id FROM contacts WHERE email = $1 LIMIT 1"#)
                .bind(&email)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        if let Some((contact_id,)) = existing {
            // An already-known contact becomes visible to the campaign's owner too — the same
            // shared identity, now linked (two businesses may share one person by design).
            if let Some(account) = campaign_account {
                crate::db::contacts::link_contact(
                    &state.db,
                    &contact_id,
                    &account,
                    "funnelswift_tag",
                )
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            }
            return Ok((
                axum::http::StatusCode::OK,
                Json(json!({
                    "status": "already_exists",
                    "contact_id": contact_id.to_string(),
                })),
            ));
        }
    }

    let contact_id = Uuid::new_v4();
    let notes = format!("funnelswift:{}:{}", req.source, req.tag.name);

    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name, notes, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())"#
    )
    .bind(contact_id)
    .bind(&first_name)
    .bind(&last_name)
    .bind(if email.is_empty() { None } else { Some(&email) })
    .bind(if phone.is_empty() { None } else { Some(&phone) })
    .bind(if company_name.is_empty() { None } else { Some(&company_name) })
    .bind(&notes)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    if let Some(account) = campaign_account {
        crate::db::contacts::link_contact(&state.db, &contact_id, &account, "funnelswift_tag")
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }

    tracing::info!(
        "tag_provision: created contact {} ({})",
        contact_id,
        first_name
    );

    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "status": "provisioned",
            "contact_id": contact_id.to_string(),
        })),
    ))
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// POST /api/v1/internal/provision-free-account — the ACCOUNT door (kanban t_3724204f)
//
// The sibling door above captures a LEAD (one `contacts` row). This one mints the account the lead
// holder can actually log into and upgrade in place: the SAME unit the self-serve signup mints —
// an `accounts` row seated on `plan_tiers(slug = the entry plan)`, with an argon2 credential and a
// generated purchase PIN — through the ONE shared writer (`crate::account_mint::mint_account`).
// Contract frozen in /opt/swift/docs/tag-to-free-account-design-2026-10-06.md §3.1.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// `admin_settings` key: the per-app master switch. Ships ABSENT, which reads as `false`.
pub const PROVISION_ENABLED_KEY: &str = "provision_from_tags_enabled";
/// `admin_settings` key: which of THIS app's tiers a tag-provisioned account is seated on.
pub const PROVISION_ENTRY_PLAN_KEY: &str = "provision_entry_plan_slug";
/// The entry-plan slug used when the setting is absent (the app's own free tier).
pub const DEFAULT_ENTRY_PLAN_SLUG: &str = "free";

/// The provisioning knobs, as the app reads them.
#[derive(Debug, Clone)]
pub struct ProvisioningSettings {
    /// Master switch. `true` is the shipped state (David 2026-10-06): the account door is
    /// OPEN by default. Set `false` to make it answer 403.
    pub enabled: bool,
    /// The tier slug a minted account is seated on. Resolved IN THIS APP (`plan_tiers`) — a
    /// sibling's plan name can never resolve here (spec §3.1 rule 1).
    pub entry_plan_slug: String,
}

/// Read both knobs. Absent keys, and values of an unexpected shape, fall back to the shipped
/// defaults — the door is ON by default; an operator turns it OFF from the console.
pub async fn read_provisioning_settings(
    db: &sqlx::PgPool,
) -> Result<ProvisioningSettings, AppError> {
    let enabled = read_setting(db, PROVISION_ENABLED_KEY).await?;
    let slug = read_setting(db, PROVISION_ENTRY_PLAN_KEY).await?;
    Ok(ProvisioningSettings {
        enabled: bool_setting(enabled.as_ref()).unwrap_or(true),
        entry_plan_slug: string_setting(slug.as_ref(), &["plan_slug", "slug", "value"])
            .unwrap_or_else(|| DEFAULT_ENTRY_PLAN_SLUG.to_string()),
    })
}

/// Persist both knobs (the admin console's only writer). `None` leaves a knob untouched, so the
/// toggle and the picker can be saved independently.
pub async fn save_provisioning_settings(
    db: &sqlx::PgPool,
    enabled: Option<bool>,
    entry_plan_slug: Option<&str>,
) -> Result<(), AppError> {
    if let Some(enabled) = enabled {
        write_setting(db, PROVISION_ENABLED_KEY, &Value::Bool(enabled)).await?;
    }
    if let Some(slug) = entry_plan_slug {
        write_setting(
            db,
            PROVISION_ENTRY_PLAN_KEY,
            &Value::String(slug.trim().to_string()),
        )
        .await?;
    }
    Ok(())
}

async fn read_setting(db: &sqlx::PgPool, key: &str) -> Result<Option<Value>, AppError> {
    let row: Option<(Value,)> = sqlx::query_as("SELECT value FROM admin_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.0))
}

async fn write_setting(db: &sqlx::PgPool, key: &str, value: &Value) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO admin_settings (key, value, description, updated_at)
         VALUES ($1, $2::jsonb, $3, NOW())
         ON CONFLICT (key) DO UPDATE SET value = $2::jsonb, updated_at = NOW()",
    )
    .bind(key)
    .bind(value.to_string())
    .bind("Tag-provisioned free accounts (FunnelSwift)")
    .execute(db)
    .await?;
    Ok(())
}

/// A boolean setting stored as a scalar, or inside an object (`{"enabled": true}`), or as the
/// string an HTML form would send.
fn bool_setting(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" | "" => Some(false),
            _ => None,
        },
        Value::Object(o) => o.get("enabled").and_then(|v| v.as_bool()),
        _ => None,
    }
}

/// A string setting stored as a scalar, or inside an object under one of `keys`.
fn string_setting(value: Option<&Value>, keys: &[&str]) -> Option<String> {
    let clean = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    match value? {
        Value::String(s) => clean(s),
        Value::Object(o) => keys
            .iter()
            .find_map(|k| o.get(*k).and_then(|v| v.as_str()).and_then(clean)),
        _ => None,
    }
}

/// Payload of the account door (spec §3.1).
#[derive(Debug, Deserialize)]
pub struct ProvisionFreeAccountRequest {
    pub source: String,
    #[serde(default)]
    pub source_tenant_id: Option<String>,
    pub tag: ProvisionFreeAccountTag,
    pub contact: ProvisionFreeAccountContact,
    /// `<funnelswift lead uuid>:<app slug>`. Recorded in the log line; the idempotency itself is
    /// the app's own (`accounts.email`/`lower(email)` — one address, one login).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProvisionFreeAccountTag {
    pub name: String,
    /// The caller's SUGGESTION. Deliberately not used to resolve the tier: the entry plan is read
    /// from THIS app's `provision_entry_plan_slug` (spec §3.1 rule 1).
    #[serde(default)]
    pub plan_slug: Option<String>,
    #[serde(default)]
    pub campaign_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ProvisionFreeAccountContact {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
}

/// POST /api/v1/internal/provision-free-account
///
/// 201 `provisioned` · 200 `already_exists` · 403 `refused` · 422 unusable address / no free tier.
pub async fn handle_provision_free_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProvisionFreeAccountRequest>,
) -> Result<Response, AppError> {
    // 1. The shared service credential, fail closed exactly as the sibling door above: an app with
    //    no configured key must never authenticate an empty header. The credential is never logged.
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = state.config.internal_sync_key.as_str();
    if expected.is_empty() || key != expected {
        tracing::warn!(
            "provision_free_account: invalid internal key (presented_len={}, configured_len={})",
            key.len(),
            expected.len()
        );
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    // 2. The master switch. Ships ON (code default true, so a FRESH INSTALL has the door open); an
    //    operator closes it from the console and this app then refuses and mints NOTHING, so a
    //    caller cannot create an account in an app whose operator has disabled the door.
    let settings = read_provisioning_settings(&state.db).await?;
    if !settings.enabled {
        tracing::info!(
            "provision_free_account: refused (provisioning disabled) tag={} source={}",
            req.tag.name,
            req.source
        );
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({ "status": "refused", "reason": "provisioning_disabled" })),
        )
            .into_response());
    }

    // 3. The address. `accounts.email` is the login identity AND the only address credentials can
    //    reach, so an empty, malformed or placeholder address is refused (422) rather than minted.
    let email =
        match crate::security::email_addr::normalize(req.contact.email.as_deref().unwrap_or("")) {
            Ok(email) => email,
            Err(reason) => {
                return Ok((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "status": "refused", "reason": reason })),
                )
                    .into_response());
            }
        };
    if is_placeholder_address(&email) {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "status": "refused",
                "reason": format!("contact.email '{email}' is a placeholder address — refusing to mint an account"),
            })),
        )
            .into_response());
    }

    // 4. The entry tier, resolved IN THIS APP and required to be free + active.
    let entry_plan_slug = settings.entry_plan_slug.clone();
    if crate::account_mint::entry_tier_id(&state.db, &entry_plan_slug)
        .await?
        .is_none()
    {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "status": "refused",
                "reason": format!(
                    "no free plan '{entry_plan_slug}' is configured in this app \
                     (a `plan_tiers` row with that slug, is_active = true and price_monthly = 0 is required)"
                ),
            })),
        )
            .into_response());
    }

    // 5. Idempotency: one address, one login — `login` resolves an account by `lower(email)`.
    if let Some(account_id) =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE lower(email) = $1 LIMIT 1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?
    {
        tracing::info!(
            "provision_free_account: already_exists email={} account={}",
            email,
            account_id
        );
        return Ok((
            StatusCode::OK,
            Json(json!({
                "status": "already_exists",
                "account_id": account_id.to_string(),
                "login_email": email,
            })),
        )
            .into_response());
    }

    // 6. Mint: the SAME unit the self-serve signup mints, through the SAME writer.
    let first = req.contact.first_name.as_deref().unwrap_or("").trim();
    let last = req.contact.last_name.as_deref().unwrap_or("").trim();
    let company = req.contact.company.as_deref().unwrap_or("").trim();
    let mut full_name = format!("{first} {last}").trim().to_string();
    if full_name.is_empty() {
        full_name = if company.is_empty() {
            email
                .split('@')
                .next()
                .unwrap_or("Account Holder")
                .to_string()
        } else {
            company.to_string()
        };
    }

    let password = crate::billing::webhooks::generate_temp_password();
    let minted = crate::account_mint::mint_account(
        &state.db,
        crate::account_mint::MintRequest {
            email: &email,
            name: &full_name,
            password: &password,
            entry_plan_slug: &entry_plan_slug,
        },
    )
    .await?;
    let account_id = minted.account_id;

    // 7. Link the lead the sibling door captured, so the account holder can see their own contact
    //    (the same shared identity + `contact_tenants` link the tag bridge uses, source
    //    'funnelswift_tag'). Best-effort: the ACCOUNT is the deliverable and is already real, so a
    //    link failure is logged and never turns a completed mint into an error response.
    if let Some(contact_id) =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM contacts WHERE lower(email) = $1 LIMIT 1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?
    {
        if let Err(e) = crate::db::contacts::link_contact(
            &state.db,
            &contact_id,
            &account_id,
            "funnelswift_tag",
        )
        .await
        {
            tracing::warn!(
                "provision_free_account: contact link failed account={} contact={}: {}",
                account_id,
                contact_id,
                e
            );
        }
    }

    tracing::info!(
        "provision_free_account: provisioned account={} email={} plan={} source={} idem={}",
        account_id,
        email,
        entry_plan_slug,
        req.source,
        req.idempotency_key.as_deref().unwrap_or("-")
    );

    // 8. The credentials mail — the app's existing template, the same one the signup door sends.
    //    The business never types a password on this path; it arrives in this message. Awaited (not
    //    spawned) so the send is ATTEMPTED before this response is returned; a failure is logged
    //    and swallowed inside the helper.
    crate::account_mint::send_credentials_email(
        &state.db, account_id, &email, &full_name, &password,
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "status": "provisioned",
            "account_id": account_id.to_string(),
            "plan_slug": entry_plan_slug,
            "login_email": email,
        })),
    )
        .into_response())
}

/// An address a human could never receive credentials at — the shape the retired tag-path fallback
/// used (`fs-provision-<uuid>@placeholder.swift.local`). Spec §3.1 rule 5: never mint on a
/// placeholder address. `email_addr::normalize` has already rejected the empty/malformed cases.
fn is_placeholder_address(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return true;
    };
    let local = local.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    local.starts_with("fs-provision-")
        || local.starts_with("provision-")
        || domain.contains("placeholder")
        || domain.ends_with(".local")
        || domain == "localhost"
        // t_a8bd2860: an RFC 2606 / RFC 6761 RESERVED address (example.com/.net/.org, .invalid,
        // .test, .example) is equally undeliverable — minting on it leaves a real, permanent
        // orphan login whose credentials mail can never arrive. Delegate to the app's own
        // canonical predicate (the same one the SEND SEAM enforces).
        || crate::security::email_addr::is_reserved_address(email)
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// Admin provisioning console — the operator's half (spec §3.3)
//
// GET  /api/v1/admin/provisioning-config — the two knobs plus this app's own free tiers.
// PUT  /api/v1/admin/provisioning-config — save them.
//
// Platform-operator only: the route sits under /api/v1/admin/*, which `security::auth::admin_guard`
// covers path-first, and the default-deny middleware covers it again. There is no second writer of
// these keys: the console is the only editor and the account door only ever READS them, so the
// shape written here is exactly the shape the door reads.
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// The writable half of the panel. Both fields are optional so the toggle and the tier picker can
/// be saved independently (`None` leaves that knob untouched).
#[derive(Debug, Deserialize)]
pub struct UpdateProvisioningSettings {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub entry_plan_slug: Option<String>,
}

pub async fn get_provisioning_config(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let settings = read_provisioning_settings(&state.db).await?;
    Ok(Json(provisioning_payload(&state, &settings).await?))
}

pub async fn update_provisioning_config(
    State(state): State<AppState>,
    Json(req): Json<UpdateProvisioningSettings>,
) -> Result<Json<Value>, AppError> {
    // The entry plan must resolve IN THIS APP to a free, active tier — the same predicate the mint
    // seats with (`account_mint::entry_tier_id`), so the picker cannot save a slug the door would
    // then refuse to mint on.
    if let Some(slug) = req.entry_plan_slug.as_deref() {
        let slug = slug.trim();
        if slug.is_empty() {
            return Err(AppError::BadRequest(
                "entry_plan_slug must not be empty".into(),
            ));
        }
        if crate::account_mint::entry_tier_id(&state.db, slug)
            .await?
            .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "'{slug}' is not one of this app's free, active plans"
            )));
        }
    }

    save_provisioning_settings(&state.db, req.enabled, req.entry_plan_slug.as_deref()).await?;

    // Answer from the STORE, never from the request: what the panel shows next is what the door
    // will actually read.
    let settings = read_provisioning_settings(&state.db).await?;
    Ok(Json(provisioning_payload(&state, &settings).await?))
}

async fn provisioning_payload(
    state: &AppState,
    settings: &ProvisioningSettings,
) -> Result<Value, AppError> {
    let plans = crate::account_mint::free_tiers(&state.db).await?;
    let entry_plan_resolves =
        crate::account_mint::entry_tier_id(&state.db, &settings.entry_plan_slug)
            .await?
            .is_some();
    Ok(json!({
        "enabled": settings.enabled,
        "entry_plan_slug": settings.entry_plan_slug,
        "entry_plan_resolves": entry_plan_resolves,
        "free_plans": plans
            .iter()
            .map(|(slug, name)| json!({ "slug": slug, "name": name }))
            .collect::<Vec<_>>(),
        "setting_keys": {
            "enabled": PROVISION_ENABLED_KEY,
            "entry_plan_slug": PROVISION_ENTRY_PLAN_KEY,
        },
        "endpoint": "/api/v1/internal/provision-free-account",
        "source_app": "funnelswift",
    }))
}
