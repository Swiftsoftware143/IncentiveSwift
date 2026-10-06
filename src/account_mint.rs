//! The ONE mint of a self-serve signup unit (kanban t_3724204f).
//!
//! Two doors mint a free account, and both call [`mint_account`]:
//!
//!   * the self-serve signup — `POST /api/v1/auth/register` ([`crate::handlers::auth_handler::register`]); and
//!   * the machine door — `POST /api/v1/internal/provision-free-account`
//!     ([`crate::handlers::tag_provision_handler::handle_provision_free_account`]), reached by
//!     FunnelSwift when a lead is tagged with `IncentiveSwift — Free`.
//!
//! Before this module the signup core lived inline in `register`, so the second door would have
//! been a SECOND writer of the same unit — and the two would drift (one seats the free tier, the
//! other forgets; one generates the purchase PIN, the other leaves `0000`; one assigns the default
//! industry, the other does not). The unit this function produces is therefore fixed:
//!
//! ```text
//!   accounts row (login identity + argon2 password_hash + role company_admin
//!                 + plan_tier_id -> plan_tiers(slug = the entry plan) + tenant_id = self
//!                 + slug + a generated purchase PIN)  +  its account_industries row
//! ```
//!
//! ## The entry plan — `plan_tiers`, never `plans`
//!
//! `accounts.plan_tier_id` has a FOREIGN KEY to `plan_tiers(id)`, so that table is the accounting
//! identity of an account's tier; `plans` is the marketing/checkout table (kanban t_329b61b2).
//! [`ENTRY_TIER_SQL`] is the one predicate for "this app has a free plan with this slug" — active
//! and `price_monthly = 0` — so a mistyped setting can never seat an account on a paid tier: the
//! mint is REFUSED instead.
//!
//! ## Scope
//!
//! This is the ONLY writer of the self-serve signup unit. The paid-checkout credential path
//! (`billing::webhooks`, a different contract) and the business-registration path
//! (`handlers::business_handler::register_business`) write their own shapes and are deliberately
//! not routed through here.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

/// The ONE predicate for "this app has a free, active plan with this slug". Shared by the mint,
/// the account door's pre-check and the admin console's validation, so the three can never
/// disagree about what a free entry plan is.
pub const ENTRY_TIER_SQL: &str =
    "SELECT id FROM plan_tiers WHERE slug = $1 AND is_active = true AND price_monthly = 0 LIMIT 1";

/// What one call to [`mint_account`] produced.
pub struct MintedAccount {
    /// The `accounts` row id. It is also the account's `tenant_id` (a standalone workspace).
    pub account_id: Uuid,
    pub plan_tier_id: Uuid,
    pub slug: String,
    /// The generated purchase PIN (`Z…`), already written to the row.
    pub purchase_pin: Option<String>,
}

/// Everything [`mint_account`] needs. `email` must ALREADY be normalised
/// (`crate::security::email_addr::normalize`); this function never re-derives it, so the value the
/// duplicate check reads is the value the INSERT stores.
pub struct MintRequest<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub password: &'a str,
    /// The tier an account is seated on. Must resolve to an ACTIVE `plan_tiers` row with
    /// `price_monthly = 0` or the mint is refused.
    pub entry_plan_slug: &'a str,
}

/// The tier id a mint will be seated on, or `None` when this app has no such free, active tier.
pub async fn entry_tier_id(db: &PgPool, entry_plan_slug: &str) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar(ENTRY_TIER_SQL)
        .bind(entry_plan_slug)
        .fetch_optional(db)
        .await?
        .flatten())
}

/// Every tier of THIS app that may be used as an entry plan: active, and free.
pub async fn free_tiers(db: &PgPool) -> Result<Vec<(String, String)>, AppError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT slug, name FROM plan_tiers WHERE is_active = true AND price_monthly = 0 \
         ORDER BY sort_order, name",
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Mint one self-serve account unit, or refuse.
///
/// A duplicate address is [`AppError::Conflict`] (the caller maps it to its own door's contract);
/// a missing free tier is [`AppError::Internal`] because at that point the app is misconfigured,
/// not the caller.
pub async fn mint_account(db: &PgPool, req: MintRequest<'_>) -> Result<MintedAccount, AppError> {
    // ── 1. one address, one login ──────────────────────────────────────────────────────────────
    // `login` resolves an account by `lower(email)`, so the duplicate test must too.
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = $1")
            .bind(req.email)
            .fetch_optional(db)
            .await?;
    if existing.is_some() {
        return Err(AppError::Conflict(
            "An account with this email already exists".to_string(),
        ));
    }

    // ── 2. the entry tier ─────────────────────────────────────────────────────────────────────
    let Some(plan_tier_id) = entry_tier_id(db, req.entry_plan_slug).await? else {
        return Err(AppError::Internal(
            "Free plan tier not configured".to_string(),
        ));
    };

    // ── 3. the accounts row ───────────────────────────────────────────────────────────────────
    let account_id = Uuid::new_v4();
    let password_hash = hash_password(req.password)?;
    let slug_base = req.email.split('@').next().unwrap_or("user");
    let slug = format!("{}-{}", slug_base, &account_id.to_string()[..8]);

    sqlx::query(
        r#"INSERT INTO accounts (id, name, email, password_hash, role, plan_tier_id, tenant_id, slug, purchase_pin)
           VALUES ($1, $2, $3, $4, 'company_admin', $5, $6, $7, '0000')"#,
    )
    .bind(account_id)
    .bind(req.name)
    .bind(req.email)
    .bind(&password_hash)
    .bind(plan_tier_id)
    .bind(account_id) // tenant_id = self (standalone workspace)
    .bind(&slug)
    .execute(db)
    .await?;

    // ── 4. the purchase PIN ───────────────────────────────────────────────────────────────────
    // Format: Z followed by a 3-digit zero-padded number (Z100, Z101, …, Z999, Z1000+).
    let next_num: Option<i32> = sqlx::query_scalar(
        "UPDATE accounts SET next_pin_number = next_pin_number + 1 WHERE id = $1 RETURNING next_pin_number - 1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let purchase_pin = match next_num {
        Some(num) => {
            let new_pin = if num < 1000 {
                format!("Z{:03}", num)
            } else {
                format!("Z{}", num)
            };
            sqlx::query("UPDATE accounts SET purchase_pin = $1 WHERE id = $2")
                .bind(&new_pin)
                .bind(account_id)
                .execute(db)
                .await?;
            Some(new_pin)
        }
        None => None,
    };

    // ── 5. the default industry ───────────────────────────────────────────────────────────────
    let industry_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM industries WHERE slug = 'general' AND is_active = true")
            .fetch_optional(db)
            .await?
            .flatten();
    if let Some(ind_id) = industry_id {
        sqlx::query(
            r#"INSERT INTO account_industries (account_id, industry_id, is_primary)
               VALUES ($1, $2, true)
               ON CONFLICT (account_id, industry_id) DO NOTHING"#,
        )
        .bind(account_id)
        .bind(ind_id)
        .execute(db)
        .await?;
    }

    Ok(MintedAccount {
        account_id,
        plan_tier_id,
        slug,
        purchase_pin,
    })
}

/// Hash a password with argon2 — the app's ONE hasher for a stored credential.
pub fn hash_password(password: &str) -> Result<String, AppError> {
    use argon2::{
        password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
        Argon2,
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("Failed to hash password: {}", e)))
}

/// The ONE credentials mail both doors send for a password THIS app generated:
/// `welcome_credentials` is the only template that carries a Password line.
///
/// A send failure is logged and swallowed — the account is already real, and a mail outage must
/// not turn a completed mint into an error response. The template lookup is tenant-scoped, so
/// `account_id` is the account the mail is FOR.
pub async fn send_credentials_email(
    db: &PgPool,
    account_id: Uuid,
    email: &str,
    name: &str,
    password: &str,
) {
    let vars = serde_json::json!({
        "name": name,
        "email": email,
        "password": password,
        "app_name": "IncentiveSwift",
        "login_url": "https://app.incentiveswift.com",
    });
    if let Err(e) =
        crate::email::send_template_email(db, Some(account_id), email, "welcome_credentials", &vars)
            .await
    {
        tracing::error!(
            "CREDENTIAL EMAIL FAILED for {} — the account exists but its generated password was never sent: {}",
            email,
            e
        );
    }
}
