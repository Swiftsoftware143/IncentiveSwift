//! Internal portfolio sync handler — receives broadcasts from CoreSwift CRM.
//! Protected by x-internal-key header, not JWT.

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use axum::{extract::State, http::HeaderMap, Json};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// RFC 4648 Base32, UPPERCASE, padding stripped — the exact encoding the fleet's derived-password
/// rule specifies. `base64` is Base64 only, so this is the one codec that must be written by hand.
fn base32_encode_upper_no_pad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buffer: u32 = 0;
    let mut bits: usize = 0;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1F) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1F) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// The fleet's derived portfolio password — deterministic, never transmitted, never logged, and
/// identical across every app that holds the same `INTERNAL_SYNC_KEY`.
///
/// ```text
/// raw      = HMAC-SHA256(key = INTERNAL_SYNC_KEY, message = "portfolio:" + portfolio_uuid_string)
/// password = Base32(raw), RFC 4648, UPPERCASE, padding stripped, FIRST 20 CHARACTERS
/// ```
///
/// Because it is a pure function of the sync key and the portfolio id, a re-sync reproduces the
/// same value — so it can never invalidate a credential the user has since changed (the derivation
/// is only applied when an account has no password at all, and an existing `password_hash` is
/// never touched).
fn derive_password(sync_key: &str, portfolio_id: Uuid) -> String {
    let message = format!("portfolio:{portfolio_id}");
    let mut mac =
        HmacSha256::new_from_slice(sync_key.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(message.as_bytes());
    let raw = mac.finalize().into_bytes();
    let b32 = base32_encode_upper_no_pad(&raw);
    b32.chars().take(20).collect()
}

/// The slug a mirrored `accounts` row must carry: the caller's own slug (the hub's contract is
/// that `body.slug` is what lands in both rows), or the app's derived unique slug when the caller
/// sent none — ONE rule, called by every arm of this door so the arms cannot drift apart.
///
/// `accounts.slug` is UNIQUE (partial, `WHERE slug IS NOT NULL`), so the random suffix is what
/// keeps two different callers apart when the caller sends no slug.
fn resolve_account_slug(name: &str, slug: &str) -> String {
    if !slug.trim().is_empty() {
        return slug.to_string();
    }
    let base: String = name
        .trim()
        .to_lowercase()
        .replace(' ', "-")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let base = if base.is_empty() {
        "account"
    } else {
        base.as_str()
    };
    let suffix: String = Uuid::new_v4().to_string().chars().take(8).collect();
    format!("{base}-{suffix}")
}

/// The parent `accounts` row a mirrored portfolio must have (kanban t_47315540), and the account id
/// the mirrored `portfolio_companies` row must point at.
///
/// It RESOLVES instead of blindly minting (kanban t_b5784899): `portfolio_companies.account_id` is
/// `REFERENCES accounts(id)` and the incoming `tenant_id` is a *caller* tenant id, not an account
/// here. Resolve in order and return the id the caller must bind:
///   1. an account already carries this caller tenant (id or tenant_id) — a re-sync, reuse it;
///   2. an account already carries this email — the same business lives here, so attach the mirror
///      to it rather than mint a twin whose INSERT would raise `23505` on `accounts_email_key`;
///   3. otherwise mint the mirror, id = the caller tenant id, so step 1 finds it next time.
pub(crate) async fn ensure_account(
    db: &sqlx::PgPool,
    aid: Uuid,
    name: &str,
    email: &str,
) -> Result<Uuid, AppError> {
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1 OR tenant_id = $1 LIMIT 1")
            .bind(aid)
            .fetch_optional(db)
            .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    if !email.is_empty() {
        let by_email: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = lower($1) LIMIT 1")
                .bind(email)
                .fetch_optional(db)
                .await?;
        if let Some(id) = by_email {
            return Ok(id);
        }
    }
    let addr = if email.is_empty() {
        format!("portfolio-sync+{aid}@sync.invalid")
    } else {
        email.to_string()
    };
    sqlx::query(
        "INSERT INTO accounts (id, name, email, plan_tier_id) VALUES ($1, $2, $3, \
         (SELECT id FROM plan_tiers WHERE slug = 'free' AND is_active = true AND price_monthly = 0 LIMIT 1)) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(aid)
    .bind(name)
    .bind(&addr)
    .execute(db)
    .await?;
    Ok(aid)
}

/// The workspace this app ALREADY holds for a portfolio company, found by the company's stable
/// identity (slug → name → email) rather than by the caller's id.
///
/// WHY (adopt-before-create, 2026-11-15): the hub changed which id it sends — it used to carry its
/// own ADMIN workspace id for every company, and now carries each company's OWN id
/// (`tenant_id == portfolio_id`, CoreSwift `src/portfolio/sync.rs`). This app already held
/// Giraudy/ZaarHub/SwiftImpact under the OLD ids, so a re-sync whose id matched nothing minted a
/// SECOND, empty account beside the real one (cleaned 2026-11-15). Matching on the identity this
/// app already stores finds the account that exists and adopts it instead of minting a duplicate.
///
/// The ACCOUNT (this app's workspace + owner login) is matched first and in full (slug → name →
/// email), because the account is the thing a re-sync must never duplicate. The
/// `portfolio_companies` row is only a FALLBACK source of an account id (a company whose account
/// this app never mirrored), never a preferred match.
struct AdoptedCompany {
    /// The existing account (accounts.id) to reuse in place of the caller's id.
    account_id: Uuid,
    /// The existing portfolio_companies.id to refresh in place (None when this app holds an
    /// account for the company but no company row yet).
    company_id: Option<Uuid>,
}

/// Resolve the `portfolio_companies` row to refresh alongside the adopted account: the row already
/// linked to it, else the row matching the company's identity (slug → name → email).
async fn resolve_company_id(
    db: &sqlx::PgPool,
    account_id: Uuid,
    name: &str,
    slug: &str,
    email: &str,
) -> Result<Option<Uuid>, AppError> {
    let by_account: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM portfolio_companies WHERE account_id = $1 LIMIT 1")
            .bind(account_id)
            .fetch_optional(db)
            .await?;
    if by_account.is_some() {
        return Ok(by_account);
    }
    if !slug.trim().is_empty() {
        let by_slug: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM portfolio_companies WHERE slug = $1 LIMIT 1")
                .bind(slug.trim())
                .fetch_optional(db)
                .await?;
        if by_slug.is_some() {
            return Ok(by_slug);
        }
    }
    if !name.trim().is_empty() {
        let by_name: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM portfolio_companies WHERE name = $1 LIMIT 1")
                .bind(name.trim())
                .fetch_optional(db)
                .await?;
        if by_name.is_some() {
            return Ok(by_name);
        }
    }
    if !email.trim().is_empty() {
        let by_email: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM portfolio_companies WHERE email = $1 LIMIT 1")
                .bind(email.trim())
                .fetch_optional(db)
                .await?;
        if by_email.is_some() {
            return Ok(by_email);
        }
    }
    Ok(None)
}

/// Find the account this app already holds for a portfolio company — slug → name → email, in the
/// `accounts` table first (the account itself) and then, as a fallback, the `portfolio_companies`
/// row's parent. Returns None only when this app holds nothing for the company, which is exactly the
/// case the create arm may mint a fresh account for.
async fn find_existing_company(
    db: &sqlx::PgPool,
    name: &str,
    slug: &str,
    email: &str,
) -> Result<Option<AdoptedCompany>, AppError> {
    // 1. The account (workspace + owner login), slug → name → email.
    if !slug.trim().is_empty() {
        let t: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE slug = $1 LIMIT 1")
            .bind(slug.trim())
            .fetch_optional(db)
            .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }
    if !name.trim().is_empty() {
        let t: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE name = $1 LIMIT 1")
            .bind(name.trim())
            .fetch_optional(db)
            .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }
    if !email.trim().is_empty() {
        let t: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = lower($1) LIMIT 1")
                .bind(email.trim())
                .fetch_optional(db)
                .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }

    // 2. Fallback — a company whose account this app never mirrored: its parent id is the account
    //    to adopt. slug → name → email.
    if !slug.trim().is_empty() {
        let t: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM portfolio_companies WHERE slug = $1 LIMIT 1",
        )
        .bind(slug.trim())
        .fetch_optional(db)
        .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }
    if !name.trim().is_empty() {
        let t: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM portfolio_companies WHERE name = $1 LIMIT 1",
        )
        .bind(name.trim())
        .fetch_optional(db)
        .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }
    if !email.trim().is_empty() {
        let t: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM portfolio_companies WHERE email = $1 LIMIT 1",
        )
        .bind(email.trim())
        .fetch_optional(db)
        .await?;
        if let Some(t) = t {
            return Ok(Some(AdoptedCompany {
                account_id: t,
                company_id: resolve_company_id(db, t, name, slug, email).await?,
            }));
        }
    }

    Ok(None)
}

/// What the provisioning half of a sync did, so the caller can see which rows changed and the
/// welcome-mail arm can send exactly once (only when the owner login was actually created).
struct ProvisionOutcome {
    /// True only when THIS sync set the account's password (i.e. the account had no login before).
    /// The welcome mail is gated on this: an account that already had a login is never re-mailed
    /// and its password is never touched.
    user_created: bool,
    /// The derived password, meaningful only when `user_created` is true (otherwise empty).
    password: String,
    /// The account's live key prefix, whether minted by this sync or already present.
    key_prefix: Option<String>,
    /// True only when THIS sync minted the key.
    key_minted: bool,
}

/// Ensure the account holds its owner login and exactly one live API key — the fleet portfolio
/// provisioning standard (2026-11-15) — inside the caller's transaction, idempotently.
///
/// In this app the ACCOUNT IS the owner user (`accounts` carries `password_hash` + `role`), so
/// "owner user missing" is read as "the account has no password hash". The role is `company_admin`,
/// the same role a manual signup gets (`account_mint::mint_account`).
///
/// * account, no password → set the DERIVED password + role `company_admin`; `user_created = true`.
/// * account + password, no key → mint the key, `user_created = false` (no re-mail, no reset).
/// * everything present → no-op; both flags false.
async fn provision_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sync_key: &str,
    account_id: Uuid,
    portfolio_id: Uuid,
    name: &str,
    email: &str,
) -> Result<ProvisionOutcome, AppError> {
    let _ = (name, email);

    // 1. The owner login. A present password hash means the login exists and is never touched.
    let has_password: bool =
        sqlx::query_scalar("SELECT password_hash IS NOT NULL FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(&mut **tx)
            .await?;

    let (user_created, password) = if has_password {
        (false, String::new())
    } else {
        let derived = derive_password(sync_key, portfolio_id);
        let hash = crate::account_mint::hash_password(&derived)?;
        sqlx::query(
            "UPDATE accounts SET password_hash = $1, role = 'company_admin', updated_at = NOW() \
                 WHERE id = $2",
        )
        .bind(&hash)
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
        (true, derived)
    };

    // 2. One live key per account, enforced by `api_keys_one_active_per_tenant`. Mint only when
    //    there is none; a re-sync never mints a second key and never revokes the one the owner
    //    already holds.
    let live_prefix: Option<String> = sqlx::query_scalar(
        "SELECT prefix FROM api_keys WHERE tenant_id = $1 AND is_active = true LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await?;

    let (key_prefix, key_minted) = match live_prefix {
        Some(prefix) => (Some(prefix), false),
        None => {
            // `mint_on` is the ONE key writer: hash + sealed copy + prefix + ledger row, all on this
            // connection so they commit or roll back with the account.
            let minted =
                crate::handlers::api_keys::mint_on(tx, account_id, account_id, "auto-provisioned")
                    .await?;
            (Some(minted.prefix), true)
        }
    };

    Ok(ProvisionOutcome {
        user_created,
        password,
        key_prefix,
        key_minted,
    })
}

/// Send the welcome mail for a just-provisioned portfolio account — best effort, AFTER commit.
///
/// The standard is explicit: a mail failure must never fail the sync (the account already exists),
/// so the failure is logged loudly and reported in the response, never returned as an error. The
/// mail goes to the PORTFOLIO email only, never to the operator.
async fn send_welcome(
    state: &AppState,
    account_id: Uuid,
    name: &str,
    email: &str,
    password: &str,
) -> bool {
    match crate::account_mint::send_credentials_email(&state.db, account_id, email, name, password)
        .await
    {
        true => {
            tracing::info!(email = %email, account = %account_id, "portfolio sync: welcome email sent");
            true
        }
        false => {
            tracing::error!(
                email = %email,
                account = %account_id,
                "portfolio sync: account provisioned but the WELCOME EMAIL FAILED"
            );
            false
        }
    }
}

/// Adopt an existing account for a portfolio re-sync: refresh its name/slug/email and its company
/// row (re-pointing the company to the adopted account), then run the SAME provisioning the create
/// arm runs (owner login if missing, one live key if missing, welcome mail only when the login was
/// actually created). Never mints a second account.
async fn adopt_existing_company(
    state: &AppState,
    found: AdoptedCompany,
    payload_pid: Option<Uuid>,
    name: &str,
    slug: &str,
    email: &str,
    can_provision: bool,
) -> AppResult<Json<Value>> {
    let account_slug = resolve_account_slug(name, slug);
    let mut tx = state.db.begin().await?;

    // 1. Refresh the existing account's name + slug. A slug that belongs to another account is
    //    refused with a 409 that NAMES it (the same refusal the create arm gives on INSERT). Email
    //    is NOT rewritten here: `accounts.email` is NOT NULL and already the account's identity.
    sqlx::query("UPDATE accounts SET name = $1, slug = $2, updated_at = NOW() WHERE id = $3")
        .bind(name)
        .bind(&account_slug)
        .bind(found.account_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
                AppError::Conflict(format!(
                    "slug '{account_slug}' is already in use by another account"
                ))
            }
            other => other.into(),
        })?;

    // 2. Refresh + re-point the company row. The pre-fix link may point at a shared/admin account;
    //    the company's OWN account is the correct scoping parent.
    match (found.company_id, payload_pid) {
        (Some(cid), _) => {
            sqlx::query(
                "UPDATE portfolio_companies SET name = $1, slug = $2, email = $3, account_id = $4, \
                 updated_at = NOW() WHERE id = $5",
            )
            .bind(name)
            .bind(&account_slug)
            .bind(email)
            .bind(found.account_id)
            .bind(cid)
            .execute(&mut *tx)
            .await?;
        }
        (None, Some(pid)) => {
            sqlx::query(
                "INSERT INTO portfolio_companies (id, account_id, name, slug, email) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, \
                 email = EXCLUDED.email, account_id = EXCLUDED.account_id, updated_at = NOW()",
            )
            .bind(pid)
            .bind(found.account_id)
            .bind(name)
            .bind(&account_slug)
            .bind(email)
            .execute(&mut *tx)
            .await?;
        }
        (None, None) => {}
    }

    // 3. Provision — owner login if missing, one live key if missing, welcome mail only if the
    //    login was actually created (the fleet standard, same as the create arm).
    let prov = if can_provision {
        match payload_pid {
            Some(pid) => {
                provision_account(
                    &mut tx,
                    &state.config.internal_sync_key,
                    found.account_id,
                    pid,
                    name,
                    email,
                )
                .await?
            }
            None => ProvisionOutcome {
                user_created: false,
                password: String::new(),
                key_prefix: None,
                key_minted: false,
            },
        }
    } else {
        ProvisionOutcome {
            user_created: false,
            password: String::new(),
            key_prefix: None,
            key_minted: false,
        }
    };

    tx.commit().await?;

    let welcome_sent = if prov.user_created {
        send_welcome(state, found.account_id, name, email, &prov.password).await
    } else {
        false
    };

    Ok(Json(json!({
        "status": if prov.user_created || prov.key_minted { "synced" } else { "already_provisioned" },
        "adopted": true,
        "account_id": found.account_id.to_string(),
        "portfolio_id": found.company_id
            .map(|c| c.to_string())
            .or_else(|| payload_pid.map(|p| p.to_string())),
        "user_created": prov.user_created,
        "key_minted": prov.key_minted,
        "welcome_sent": welcome_sent,
        "key_prefix": prov.key_prefix,
    })))
}

/// POST /api/v1/internal/portfolio-sync
/// Accepts x-internal-key header for authentication.
pub async fn portfolio_sync_internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> AppResult<Json<Value>> {
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // An empty configured key must never authenticate a caller (kanban t_de6f2986).
    if state.config.internal_sync_key.is_empty() || key != state.config.internal_sync_key {
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("create");
    let portfolio_id = body
        .get("portfolio_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let tenant_id = body
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // IncentiveSwift uses account_id not tenant_id for portfolio_companies
    let account_id = tenant_id;

    // Provisioning needs an address (users/login is the account itself; a password + welcome mail
    // need a deliverable address). A payload without one stays a legal mirror — the account is just
    // left as the contact-less shell it always was, and the missing login is filled by a later sync
    // that carries the address.
    let can_provision = !email.trim().is_empty();

    // ── adopt-before-create (2026-11-15) ────────────────────────────────────────────────────────
    // A re-sync of a company this app ALREADY holds must adopt the existing account, never mint a
    // duplicate beside it. The hub changed which id it sends (the company's OWN id instead of its
    // admin-workspace id), so the caller's id no longer matches the id this app stored; the match is
    // the company's stable identity — slug → name → email. Only when NOTHING matches does the
    // create/update arm below mint a fresh account.
    if action == "create" || action == "update" {
        if let Some(found) = find_existing_company(&state.db, &name, &slug, &email).await? {
            return adopt_existing_company(
                &state,
                found,
                portfolio_id,
                &name,
                &slug,
                &email,
                can_provision,
            )
            .await;
        }
    }

    match action {
        "create" => {
            if let (Some(pid), Some(aid)) = (portfolio_id, account_id) {
                let parent_id = ensure_account(&state.db, aid, &name, &email).await?;
                let account_slug = resolve_account_slug(&name, &slug);
                let mut tx = state.db.begin().await?;
                sqlx::query(
                    "UPDATE accounts SET name = $1, slug = $2, updated_at = NOW() WHERE id = $3",
                )
                .bind(&name)
                .bind(&account_slug)
                .bind(parent_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| match e {
                    sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
                        AppError::Conflict(format!(
                            "slug '{account_slug}' is already in use by another account"
                        ))
                    }
                    other => other.into(),
                })?;
                sqlx::query(
                    "INSERT INTO portfolio_companies (id, account_id, name, slug, email) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, \
                     email = EXCLUDED.email, account_id = EXCLUDED.account_id, updated_at = NOW()",
                )
                .bind(pid)
                .bind(parent_id)
                .bind(&name)
                .bind(&account_slug)
                .bind(&email)
                .execute(&mut *tx)
                .await?;
                let prov = if can_provision {
                    provision_account(
                        &mut tx,
                        &state.config.internal_sync_key,
                        parent_id,
                        pid,
                        &name,
                        &email,
                    )
                    .await?
                } else {
                    ProvisionOutcome {
                        user_created: false,
                        password: String::new(),
                        key_prefix: None,
                        key_minted: false,
                    }
                };
                tx.commit().await?;
                let welcome_sent = if prov.user_created {
                    send_welcome(&state, parent_id, &name, &email, &prov.password).await
                } else {
                    false
                };
                return Ok(Json(json!({
                    "status": if prov.user_created || prov.key_minted { "synced" } else { "already_provisioned" },
                    "account_id": parent_id.to_string(),
                    "portfolio_id": pid.to_string(),
                    "user_created": prov.user_created,
                    "key_minted": prov.key_minted,
                    "welcome_sent": welcome_sent,
                    "key_prefix": prov.key_prefix,
                })));
            }
        }
        "update" => {
            if let Some(pid) = portfolio_id {
                let rows = sqlx::query(
                    "UPDATE portfolio_companies SET name = $1, slug = $2, email = $3, updated_at = NOW() \
                     WHERE id = $4",
                )
                .bind(&name)
                .bind(&slug)
                .bind(&email)
                .bind(pid)
                .execute(&state.db)
                .await?;
                if rows.rows_affected() == 0 {
                    // Nothing matched, so this app holds no company row for `pid`: this is the arm
                    // that catches up after a `create` broadcast this app MISSED. Mirror the caller's
                    // account exactly as the create arm of this same door does.
                    let Some(aid) = account_id else {
                        return Err(AppError::BadRequest(
                            "update for a portfolio company this app has no row for requires 'tenant_id' to mirror as its parent".into(),
                        ));
                    };
                    let parent_id = ensure_account(&state.db, aid, &name, &email).await?;
                    let account_slug = resolve_account_slug(&name, &slug);
                    let mut tx = state.db.begin().await?;
                    sqlx::query(
                        "UPDATE accounts SET name = $1, slug = $2, updated_at = NOW() WHERE id = $3",
                    )
                    .bind(&name)
                    .bind(&account_slug)
                    .bind(parent_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| match e {
                        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
                            AppError::Conflict(format!(
                                "slug '{account_slug}' is already in use by another account"
                            ))
                        }
                        other => other.into(),
                    })?;
                    sqlx::query(
                        "INSERT INTO portfolio_companies (id, account_id, name, slug, email) \
                         VALUES ($1, $2, $3, $4, $5) \
                         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, \
                         email = EXCLUDED.email, account_id = EXCLUDED.account_id, updated_at = NOW()",
                    )
                    .bind(pid)
                    .bind(parent_id)
                    .bind(&name)
                    .bind(&account_slug)
                    .bind(&email)
                    .execute(&mut *tx)
                    .await?;
                    let prov = if can_provision {
                        provision_account(
                            &mut tx,
                            &state.config.internal_sync_key,
                            parent_id,
                            pid,
                            &name,
                            &email,
                        )
                        .await?
                    } else {
                        ProvisionOutcome {
                            user_created: false,
                            password: String::new(),
                            key_prefix: None,
                            key_minted: false,
                        }
                    };
                    tx.commit().await?;
                    let welcome_sent = if prov.user_created {
                        send_welcome(&state, parent_id, &name, &email, &prov.password).await
                    } else {
                        false
                    };
                    return Ok(Json(json!({
                        "status": if prov.user_created || prov.key_minted { "synced" } else { "already_provisioned" },
                        "account_id": parent_id.to_string(),
                        "portfolio_id": pid.to_string(),
                        "user_created": prov.user_created,
                        "key_minted": prov.key_minted,
                        "welcome_sent": welcome_sent,
                        "key_prefix": prov.key_prefix,
                    })));
                }
                // The company row already existed: this is the "update when provisioning is
                // missing" arm. Provision against the company's OWN account (the row's scoping key,
                // never a caller-supplied guess).
                let existing_account: Option<Uuid> =
                    sqlx::query_scalar("SELECT account_id FROM portfolio_companies WHERE id = $1")
                        .bind(pid)
                        .fetch_optional(&state.db)
                        .await?;
                let Some(aid) = existing_account else {
                    return Ok(Json(json!({ "status": "synced" })));
                };
                let mut tx = state.db.begin().await?;
                let prov = if can_provision {
                    provision_account(
                        &mut tx,
                        &state.config.internal_sync_key,
                        aid,
                        pid,
                        &name,
                        &email,
                    )
                    .await?
                } else {
                    ProvisionOutcome {
                        user_created: false,
                        password: String::new(),
                        key_prefix: None,
                        key_minted: false,
                    }
                };
                tx.commit().await?;
                let welcome_sent = if prov.user_created {
                    send_welcome(&state, aid, &name, &email, &prov.password).await
                } else {
                    false
                };
                return Ok(Json(json!({
                    "status": if prov.user_created || prov.key_minted { "synced" } else { "already_provisioned" },
                    "account_id": aid.to_string(),
                    "portfolio_id": pid.to_string(),
                    "user_created": prov.user_created,
                    "key_minted": prov.key_minted,
                    "welcome_sent": welcome_sent,
                    "key_prefix": prov.key_prefix,
                })));
            }
        }
        "delete" => {
            if let Some(pid) = portfolio_id {
                sqlx::query("DELETE FROM portfolio_companies WHERE id = $1")
                    .bind(pid)
                    .execute(&state.db)
                    .await?;
            }
        }
        _ => return Err(AppError::BadRequest("Invalid action".into())),
    }

    Ok(Json(json!({ "status": "synced" })))
}

#[cfg(test)]
mod tests {
    use super::{base32_encode_upper_no_pad, derive_password};

    /// The derivation is a pure function of the sync key and the portfolio id: deterministic,
    /// 20 chars, and Base32-uppercase. The fleet's own verification token (BV Premiere's derived
    /// password hashes to `sha256` beginning `a1647a97a644`) is asserted against the LIVE key in
    /// the acceptance probe, not here (the sync key is a secret and must not be embedded in tests).
    #[test]
    fn derived_password_is_deterministic_uppercase_base32_of_20_chars() {
        let sync_key = "0123456789abcdef0123456789abcdef0123456789abcdef"; // length matches live key
        let portfolio_id = uuid::Uuid::nil();
        let password = derive_password(sync_key, portfolio_id);
        assert_eq!(password.len(), 20);
        assert!(
            password
                .chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "Base32-uppercase alphabet only: {password}"
        );
        assert_eq!(password, derive_password(sync_key, portfolio_id));
        let other = derive_password(sync_key, uuid::Uuid::new_v4());
        assert_ne!(password, other);
    }

    /// RFC 4648 Base32, uppercase, no padding — pinned against the standard's RFC 4648 test vector
    /// (`foobar` → `MZXW6YTBOI`).
    #[test]
    fn base32_is_rfc_4648_uppercase_no_pad() {
        assert_eq!(base32_encode_upper_no_pad(b"f"), "MY");
        assert_eq!(base32_encode_upper_no_pad(b"fo"), "MZXQ");
        assert_eq!(base32_encode_upper_no_pad(b"foo"), "MZXW6");
        assert_eq!(base32_encode_upper_no_pad(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode_upper_no_pad(b"foobar"), "MZXW6YTBOI");
    }
}
