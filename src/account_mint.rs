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
    /// The `X-Swift-Harness` marker, when the door that called this mint is the SERVED,
    /// unauthenticated signup and the caller sent one (kanban t_4a744754). Read from the request
    /// HEADER only (`crate::probe_harness::from_headers`) — never from the body and never from a
    /// query field. `None` mints a row byte-identical to every row minted before the column
    /// existed, which is what every authenticated door passes.
    pub probe_harness: Option<&'a str>,
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
    //
    // The NAME is bounded first, because this is the ONE writer both doors pass through (the
    // served signup at `auth_handler::register` and the cross-app tag door at
    // `tag_provision_handler`), and `accounts.name` is TEXT — so an over-long name used to store
    // silently. That is the same defect the profile screen hit as a raw 500, with a quieter face:
    // the console header prints it, and `update_profile` now caps the field at 255, so a stored
    // 5000-character name could not be re-saved at all. Refused with the field and the cap named;
    // measured LIVE 2026-10-16: 0 of the 9 live accounts carry a longer field, so nothing existing
    // is affected.
    // A BLANK name is refused here too, and the value this returns is what gets bound. `check_len`
    // let a blank through (a blank is legal for the CLEARABLE profile fields it was written for),
    // so this door answered 200 to `POST /api/v1/auth/register {"name":"   "}` and stored a
    // 3-space name — the console header and the welcome mail's greeting were then empty and the
    // row looked exactly like a real one. `normalize_name` is the SAME arm `update_profile`
    // (auth_handler.rs:596) already enforces, and it returns the TRIMMED value the caller must
    // bind. Measured LIVE 2026-10-16: exactly 1 blank-name account fleet-wide (the probe that
    // found this) and 0 carrying control characters, so the tightened arm changes nothing that
    // already exists.
    let name =
        crate::security::display_name::normalize_name(req.name).map_err(AppError::BadRequest)?;

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

    // ── 3. the account unit — ONE transaction (measured defect, 2026-10-12) ─────────────
    // Steps 3-5 used to run as separate statements on separate pooled connections, so a failure
    // after the accounts INSERT left a half-minted account behind while the caller got a
    // `500 {"code":500,"error":"Internal server error"}` — and the retry then hit the duplicate
    // pre-check, so the address was burned with nobody able to sign in. Measured live at the app
    // origin: eight concurrent POST /api/v1/auth/register with ONE address produced SEVEN 500s.
    // The unit is now ONE transaction, committed only once the account is complete.
    let account_id = Uuid::new_v4();
    let password_hash = hash_password(req.password)?;
    let slug_base = req.email.split('@').next().unwrap_or("user");
    let slug = format!("{}-{}", slug_base, &account_id.to_string()[..8]);

    let mut tx = db.begin().await?;

    // `accounts_email_key` is the real guard against a duplicate address; the SELECT at step 1 is
    // only a fast path and cannot make the check atomic. Two racing signups for one address both
    // pass it, so the loser must be told the truth — the same Conflict step 1 returns — instead of
    // the 500 a raw `?` turned the unique violation into.
    let account_insert = sqlx::query(
        r#"INSERT INTO accounts (id, name, email, password_hash, role, plan_tier_id, tenant_id, slug, purchase_pin, probe_harness)
           VALUES ($1, $2, $3, $4, 'company_admin', $5, $6, $7, '0000', $8)"#,
    )
    .bind(account_id)
    .bind(&name)
    .bind(req.email)
    .bind(&password_hash)
    .bind(plan_tier_id)
    .bind(account_id) // tenant_id = self (standalone workspace)
    .bind(&slug)
    .bind(req.probe_harness)
    .execute(&mut *tx)
    .await;
    if let Err(e) = account_insert {
        tx.rollback().await.ok();
        if let sqlx::Error::Database(db_err) = &e {
            if db_err.constraint() == Some("accounts_email_key") {
                return Err(AppError::Conflict(
                    "An account with this email already exists".to_string(),
                ));
            }
        }
        return Err(e.into());
    }

    // ── 4. the purchase PIN ───────────────────────────────────────────────────────────────────────────────────
    // Format: Z followed by a 3-digit zero-padded number (Z100, Z101, …, Z999, Z1000+).
    let next_num: Option<i32> = sqlx::query_scalar(
        "UPDATE accounts SET next_pin_number = next_pin_number + 1 WHERE id = $1 RETURNING next_pin_number - 1",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
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
                .execute(&mut *tx)
                .await?;
            Some(new_pin)
        }
        None => None,
    };

    // ── 5. the default industry ────────────────────────────────────────────────────────────────────────────
    let industry_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM industries WHERE slug = 'general' AND is_active = true")
            .fetch_optional(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
    }

    // The account row, its purchase PIN and its default industry are now ONE unit of work: the
    // commit lands only when all three are complete. The credentials mail stays after the commit
    // (a published outcome, not an account row).
    tx.commit().await?;

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
///
/// ## The RETURN VALUE is the outcome, and it is what the signup page reads (kanban t_41da58ca)
///
/// `true` means the mail actually left: the provider accepted it. `false` means this customer has an
/// account whose generated password is in nobody's inbox — a send error, or a recipient the sender
/// deliberately withholds (a reserved/harness address). Both are real and both must be reportable:
/// the signup page used to promise "check your email" unconditionally, so a failure stranded the
/// customer hunting a message that was never sent.
///
/// WHY `recipient_is_undeliverable` IS CONSULTED SEPARATELY: for a withheld recipient the choke
/// point in `crate::email` records `withheld` and returns `Ok(())` (nothing was attempted, so
/// nothing may read as a provider failure). Keying this on the `Result` alone would report `true`
/// for a message that was never dispatched — the exact misreport this card is about.
pub async fn send_credentials_email(
    db: &PgPool,
    account_id: Uuid,
    email: &str,
    name: &str,
    password: &str,
) -> bool {
    let vars = serde_json::json!({
        "name": name,
        "email": email,
        "password": password,
        "app_name": "IncentiveSwift",
        "login_url": "https://app.incentiveswift.com",
    });
    // The send is attempted EITHER WAY: it is what writes the `withheld` history row the operator's
    // Email panel reads, so skipping it for an undeliverable recipient would hide the reason.
    let withheld = crate::email::recipient_is_undeliverable(email);
    match crate::email::send_template_email(
        db,
        Some(account_id),
        email,
        "welcome_credentials",
        &vars,
    )
    .await
    {
        Ok(()) if withheld => {
            tracing::info!(
                "credentials email for {} was WITHHELD — the account exists and the caller was told no mail was sent",
                email
            );
            false
        }
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                "CREDENTIAL EMAIL FAILED for {} — the account exists but its generated password was never sent: {}",
                email,
                e
            );
            false
        }
    }
}
