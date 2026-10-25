//! Auth handlers — login, me, profile, password, profile picture, forgot/reset password.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::security::email_addr;
use crate::state::AppState;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Path, Request, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Login request body.
#[derive(Deserialize)]
pub struct LoginInput {
    pub email: String,
    pub password: String,
}

/// Password change request body.
#[derive(Deserialize)]
pub struct ChangePasswordInput {
    pub current_password: String,
    pub new_password: String,
}

/// Forgot password request body.
#[derive(Deserialize)]
pub struct ForgotPasswordInput {
    pub email: String,
}

/// Reset password request body.
#[derive(Deserialize)]
pub struct ResetPasswordInput {
    pub token: String,
    pub new_password: String,
}

/// Register request body.
#[derive(Deserialize)]
pub struct RegisterInput {
    pub email: String,
    /// OPTIONAL since 2026-10-01. David's rule: *"users have a option to become an affiliate… the
    /// system for all the software generates a password that works for each new user"* — a signup is
    /// NAME + EMAIL and the credential is minted here and emailed, never asked for. A caller that still
    /// supplies one (an API client, a test, a legacy form) keeps exactly that password.
    #[serde(default)]
    pub password: String,
    pub name: Option<String>,
    pub referral_code: Option<String>,
    pub industry_slug: Option<String>,
    /// The paid tier the visitor picked on the public pricing page (`/signup?plan=pro`). Accepted
    /// under BOTH names for the same reason the sibling door does (cron 2026-10-21 there): the page
    /// speaks `plan`, API callers speak `plan_slug`, and a field only one caller can fill silently
    /// does nothing. Recorded as the same `plan_request` row the dashboard's upgrade button files
    /// (cron 2026-10-23) — before this, the pick was dropped and the operator never knew.
    #[serde(default, alias = "plan_slug")]
    pub plan: Option<String>,
}

/// POST /api/v1/auth/register — self-service signup, assigns Free plan.
///
/// `headers` is here for ONE reason: the served signup is the fleet's only anonymous door that
/// mints a workspace, so it is the only door that records the `X-Swift-Harness` marker a test
/// harness may send (kanban t_4a744754). A `HeaderMap` is a `FromRequestParts` extractor, so it
/// must be listed BEFORE the `Json` body extractor, which consumes the request.
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterInput>,
) -> Result<Json<Value>, AppError> {
    // Attribution for a workspace this route is about to mint (extended 2026-10-17): the
    // `X-Swift-Harness` HEADER names the harness when the caller sends one; otherwise the
    // recipient's own harness/reserved domain decides (`probe_harness::for_signup`, which reuses
    // `security::probe_addr` — the predicate that suppresses those sends). Neither arm reads the
    // body, and a real customer's domain matches neither, so their account row is unchanged.
    // (Computed below, once the address has been normalised.)
    // ── Address boundary (kanban t_d7ef4a88, class t_4722a331) ─────────────────────────────
    // FIRST, before any SELECT and long before any INSERT. `accounts.email` is both the login
    // identity and the only address the generated-credentials mail can ever reach; the handler
    // used to bind `body.email` verbatim after an `is_empty()` check, so the literal string `bad`
    // became a real, permanently-unreachable account. Normalises (trim + lowercase) as well as
    // validates, and the normalised value is what is checked, derived from, stored, put in the
    // token and mailed.
    let email = email_addr::normalize(&body.email).map_err(AppError::BadRequest)?;
    // Harness attribution for the account this door is about to mint — the header arm names the
    // harness, the address arm derives it from the recipient's own harness/reserved domain.
    let probe_harness = crate::probe_harness::for_signup(&headers, &email);
    // A supplied password is still honoured, but it is no longer required: the generated one below is
    // what David's model asks for, and it is what the credential email binds.
    if !body.password.is_empty() && body.password.len() < 6 {
        return Err(AppError::BadRequest(
            "Password must be at least 6 characters".to_string(),
        ));
    }
    let generated_password: Option<String> = if body.password.is_empty() {
        Some(crate::billing::webhooks::generate_temp_password())
    } else {
        None
    };
    // The one plaintext that will be hashed AND emailed — so the password a new user is sent is
    // provably the password their account accepts.
    let plaintext_password = generated_password
        .clone()
        .unwrap_or_else(|| body.password.clone());

    // The account UNIT is minted by the app's ONE signup writer (`account_mint::mint_account`) —
    // the same function the FunnelSwift tag door calls — so the two doors cannot drift. It owns the
    // duplicate check, the free-tier resolution (`plan_tiers`, the table `accounts.plan_tier_id`
    // actually has the FK to), the `accounts` row, the generated purchase PIN and the default
    // industry. `register` keeps only what is signup-specific: the JWT, the referral bonus, the
    // welcome mail and this door's response shape.
    let name = body
        .name
        .unwrap_or_else(|| email.split('@').next().unwrap_or("User").to_string());
    let minted = match crate::account_mint::mint_account(
        &state.db,
        crate::account_mint::MintRequest {
            email: &email,
            name: &name,
            password: &plaintext_password,
            // The self-serve signup is always the platform's own free tier, never a setting.
            entry_plan_slug: "free",
            probe_harness: probe_harness.as_deref(),
        },
    )
    .await
    {
        Ok(m) => m,
        // A taken address is a CONFLICT (409) — the answer every sibling app's signup door gives
        // (FunnelSwift 409, WorkflowSwift 409, ADASwift 409, MissedCall 409, and CoreSwift pinned
        // 409 as the fleet standard in kanban t_1f6e509c). This door historically downgraded the
        // Conflict to 400, so a client that tells "already registered" apart from "bad input" by
        // STATUS was wrong here and only here. The machine door's own pre-check is unchanged.
        Err(e) => return Err(e),
    };
    let account_id = minted.account_id;

    // Generate JWT
    let token = create_jwt(
        &account_id.to_string(),
        &email,
        "company_admin",
        &state.config.jwt_secret,
    )?;

    // Award referral bonus if referral_code provided
    if let Some(ref ref_code) = body.referral_code {
        // Look up referrer by their referral code
        let referrer = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
            r#"SELECT id, program_id, contact_id, member_since, last_checkin_at,
                      COALESCE(points_balance, 0) AS points_balance, COALESCE(lifetime_points, 0) AS lifetime_points
               FROM loyalty_members WHERE referral_code = $1"#,
        )
        .bind(ref_code)
        .fetch_optional(&state.db)
        .await?;

        if let Some(program_member) = referrer {
            // Get program for referral_bonus amount
            let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
                r#"SELECT id, campaign_id, name, slug, recognition_method,
                          points_per_checkin, max_checkins_per_day,
                          point_decay_days, is_active, created_at,
                          tiers_enabled, milestones_enabled, streak_enabled,
                          streak_bonus, streak_days, referral_bonus, birthday_bonus,
                          points_expire_days, social_share_points, points_per_visit,
                          currency_name, currency_icon, currency_color
                   FROM loyalty_programs WHERE id = $1 AND is_active = true"#,
            )
            .bind(program_member.program_id)
            .fetch_optional(&state.db)
            .await?;

            if let Some(ref prog) = program {
                let bonus = if prog.referral_bonus > 0 {
                    prog.referral_bonus as i64
                } else {
                    50i64
                };

                // Award points to referrer
                sqlx::query(
                    r#"UPDATE loyalty_members
                       SET points_balance = COALESCE(points_balance, 0) + $1,
                           lifetime_points = COALESCE(lifetime_points, 0) + $1
                       WHERE id = $2"#,
                )
                .bind(bonus)
                .bind(program_member.id)
                .execute(&state.db)
                .await?;

                // Record the referral action
                let metadata = serde_json::json!({
                    "type": "signup_referral",
                    "referee_email": email,
                    "bonus": bonus
                });
                sqlx::query(
                    r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
                       VALUES ($1, 'referral_signup', $2, $3::jsonb)"#
                )
                .bind(program_member.id)
                .bind(bonus)
                .bind(metadata.to_string())
                .execute(&state.db)
                .await?;

                // Award points to referee (new user) — create a member record for them
                // First find or create a contact for this new user
                let contact_id = sqlx::query_scalar::<_, Uuid>(
                    r#"SELECT id FROM contacts WHERE lower(email) = lower($1) LIMIT 1"#,
                )
                .bind(&email)
                .fetch_optional(&state.db)
                .await?;

                if let Some(cid) = contact_id {
                    // Check if they already have a membership in this program
                    let existing_member = sqlx::query_scalar::<_, i64>(
                        r#"SELECT COUNT(*) FROM loyalty_members WHERE contact_id = $1 AND program_id = $2"#
                    )
                    .bind(cid)
                    .bind(program_member.program_id)
                    .fetch_one(&state.db)
                    .await?;

                    if existing_member == 0 {
                        // Create member record for referee with welcome bonus
                        sqlx::query(
                            r#"INSERT INTO loyalty_members (program_id, contact_id, points_balance, lifetime_points, referral_code)
                               VALUES ($1, $2, $3, $3, $4)"#
                        )
                        .bind(program_member.program_id)
                        .bind(cid)
                        .bind(bonus / 2) // Referee gets half the bonus
                        .bind(format!("ref-{}", &Uuid::new_v4().to_string()[..8]))
                        .execute(&state.db)
                        .await?;
                    }
                }
            }
        }
    }

    // Account signup is not a lead-capture event (there is no contact yet); the
    // legacy account-as-contact CoreSwift push that lived here was removed. Captured
    // leads reach CoreSwift through the shared push in the entry / IQS paths.

    // Send welcome email
    let wl_pool = state.db.clone();
    let wl_email = email.clone();
    let wl_name = name.clone();
    // The account this mail is FOR — the template lookup is tenant-scoped (kanban t_0fb81177).
    let wl_account = account_id;
    let wl_generated = generated_password.clone();
    tokio::spawn(async move {
        // `welcome_credentials` is the only template that carries a Password line, so it is sent
        // exactly when THIS handler minted the password — through the SAME shared writer the
        // tag-provision door uses, so both doors mail one rendering of the credential. A caller
        // that brought its own password keeps the plain `welcome` mail, because mailing a password
        // the user chose seconds ago is the thing the fleet's other credential mails avoid.
        match wl_generated {
            Some(pw) => {
                crate::account_mint::send_credentials_email(
                    &wl_pool, wl_account, &wl_email, &wl_name, &pw,
                )
                .await;
            }
            None => {
                let vars = serde_json::json!({
                    "name": wl_name,
                    "email": wl_email,
                    "password": "",
                    "app_name": "IncentiveSwift",
                    "login_url": "https://app.incentiveswift.com"
                });
                if let Err(e) = crate::email::send_template_email(
                    &wl_pool,
                    Some(wl_account),
                    &wl_email,
                    "welcome",
                    &vars,
                )
                .await
                {
                    tracing::error!("Welcome email failed for {}: {}", wl_email, e);
                }
            }
        }
    });

    // The tier the visitor picked on the pricing page, kept as a WORKABLE request (cron 2026-10-23).
    // The card carries `?plan=pro` into the signup page and that page told them "You picked Pro", but
    // the POST carried name+email only — so the pick died here and the operator's Plan requests view
    // never saw a paid interest from a fresh signup. Filed as the same `source='plan_request'` row the
    // dashboard's upgrade button creates, which is what that view reads and what its Resolve action
    // switches the workspace with. Best-effort: a free/unknown/retired slug files nothing and the
    // account still succeeds — the workspace always starts on Free.
    let plan_request = match body
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(picked) => {
            match crate::handlers::support_tickets::record_signup_plan_request(
                &state.db, account_id, picked,
            )
            .await
            {
                Ok(filed) => filed,
                Err(e) => {
                    tracing::warn!("signup plan pick '{picked}' was not recorded: {e}");
                    None
                }
            }
        }
        None => None,
    };

    Ok(Json(json!({
        "token": token,
        "user": {
            "id": account_id,
            "email": email,
            "name": name,
            "role": "company_admin",
            "plan": "free",
        },
        // What the signup page reads to tell the visitor their pick was received (null when they
        // picked Free or named no tier): `{plan_slug, plan_name}`.
        "plan_request": match plan_request {
            Some((slug, plan_name)) => json!({ "plan_slug": slug, "plan_name": plan_name }),
            None => Value::Null,
        },
    })))
}

/// Create a signed JWT token for an authenticated user.
fn create_jwt(account_id: &str, email: &str, role: &str, secret: &str) -> Result<String, AppError> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let header = json!({
        "alg": "HS256",
        "typ": "JWT",
    });

    let now = chrono::Utc::now().timestamp();
    let payload = json!({
        "sub": account_id,
        "email": email,
        "role": role,
        "iat": now,
        "exp": now + 86400, // 24 hours
    });

    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&header).unwrap().as_bytes());
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&payload).unwrap().as_bytes());

    let message = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Failed to create HMAC".to_string()))?;
    mac.update(message.as_bytes());
    let sig = mac.finalize().into_bytes();

    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);

    Ok(format!("{}.{}", message, sig_b64))
}

/// Verify a password against a stored hash (argon2 or bcrypt).
fn verify_password(password: &str, hash: &str) -> bool {
    // Try argon2 first
    if let Ok(parsed) = PasswordHash::new(hash) {
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
        {
            return true;
        }
    }
    // Fallback: try bcrypt for backward compatibility
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// Hash a password with argon2 — delegated to the app's ONE hasher
/// ([`crate::account_mint::hash_password`]), which the signup mint and the tag door also use.
fn hash_password(password: &str) -> Result<String, AppError> {
    crate::account_mint::hash_password(password)
}

/// POST /api/v1/auth/login
/// Lazily-generated dummy Argon2 hash, so a sign-in for an address with NO account costs the
/// same verify as a real one (cron 2026-10-25: measured live at ~1.7 ms for an unknown address
/// against ~23.0 ms for a known one — a 13x gap that named which addresses are customers).
fn dummy_password_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(b"incentiveswift-timing-equalizer", &salt)
            .map(|h| h.to_string())
            .unwrap_or_default()
    })
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginInput>,
) -> Result<Json<Value>, AppError> {
    // Failed-sign-in throttle (cron 2026-10-25): before this, twenty-two consecutive wrong
    // passwords were answered 401 with no limit of any kind. The counter is keyed on the
    // CLIENT ADDRESS the fronting proxy reports, never on the account: a per-account lockout
    // would hand an attacker a denial of service against any known customer. See
    // `crate::login_throttle` (which also explains why nginx's 20 r/s is not enough).
    let throttle_key = crate::login_throttle::client_identity(&headers);
    if let Err(wait) = state.login_throttle.check(&throttle_key) {
        tracing::warn!(
            "sign-in paused for {}: failed-attempt allowance spent ({}s left)",
            throttle_key,
            wait
        );
        return Err(AppError::TooManyAttempts {
            retry_after_secs: wait,
        });
    }
    // Look up account by email
    let row = sqlx::query(
        r#"SELECT id, email, password_hash, role
           FROM accounts WHERE lower(email) = $1"#,
    )
    .bind(email_addr::lookup_key(&body.email))
    .fetch_optional(&state.db)
    .await?;

    // One closure for every failed-credential answer, so the counter can never be forgotten
    // on one arm and not another.
    let failed = |why: &str| {
        state.login_throttle.record_failure(&throttle_key);
        AppError::Unauthorized(why.to_string())
    };

    // ONE Argon2 verify either way: against the stored hash when it is a usable PHC string,
    // otherwise against the dummy (see `dummy_password_hash`). A stored hash that is missing
    // or unparsable is NOT a credential — no password can match it — so it answers the same
    // generic 401 and never a 500 that announces a broken row.
    let stored_hash: Option<String> = row
        .as_ref()
        .and_then(|r| r.get::<Option<String>, _>("password_hash"));
    let hash_to_check = match stored_hash {
        Some(hash) if PasswordHash::new(&hash).is_ok() => hash,
        _ => dummy_password_hash().to_string(),
    };
    let valid = verify_password(&body.password, &hash_to_check);

    let row = match (row, valid) {
        (Some(row), true) => row,
        _ => return Err(failed("Invalid email or password")),
    };

    // A correct password clears this address's failures, so a customer who mistyped twice
    // inside the window is not left counting down.
    state.login_throttle.clear(&throttle_key);

    let account_id: Uuid = row.get("id");
    let email: String = row.get("email");
    let role: String = row.get("role");

    // Generate JWT
    let token = create_jwt(
        &account_id.to_string(),
        &email,
        &role,
        &state.config.jwt_secret,
    )?;

    Ok(Json(json!({
        "token": token,
        "user": {
            "id": account_id,
            "email": email,
            "role": role,
        }
    })))
}

/// GET /api/v1/auth/me
///
/// The signed-in ACCOUNT with everything the console's Profile screen and account panel render:
/// `plan_name` (the real tier — never the literal word "User"), `username`, `company` and
/// `avatar_url`. `company`/`username` live on `accounts` and `avatar_url` is DERIVED from the
/// presence of the `user_avatars` row (migration zz_is12) rather than stored, so no second writer
/// can leave a URL pointing at a picture that is not there.
#[derive(Deserialize)]
pub struct UpdateProfileInput {
    pub name: Option<String>,
    pub username: Option<String>,
    pub company: Option<String>,
    /// Retained from before this card: the industry selector is a real, plan-gated capability of
    /// this app (`features::industry_limit`), and the fleet profile contract does not remove it.
    pub industry_slug: Option<String>,
}

pub async fn me(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    let row = sqlx::query(
        r#"-- The tier shape lives in `plan_tiers`: accounts.plan_tier_id has an FK to plan_tiers(id), and
           -- features.rs / access::feature_gate resolve max_campaigns, max_entries_per_month and the
           -- enabled feature keys off that same table + tier_features. `plans` is the marketing/checkout
           -- table and never had max_campaigns / max_entries_per_month / an enabled-key JSONB, so this
           -- statement 500'd on EVERY call for EVERY account (plain-statement column drift, kanban
           -- t_c7ba0473; same class as t_cf7469bb, which missed this one because it is a JOIN).
           -- GET /api/v1/auth/me and PUT /api/v1/auth/profile share this statement verbatim.
           SELECT a.name, a.email, a.role, a.plan_tier_id,
                  a.username, a.company,
                  (SELECT ua.updated_at FROM user_avatars ua WHERE ua.user_id = a.id) AS avatar_updated_at,
                  t.name as plan_name,
                  t.slug as plan_slug,
                  COALESCE((
                      -- An included BOOLEAN feature carries NO limit_value (the row is
                      -- enabled=true, limit_value NULL), so the old COALESCE turned every one of
                      -- them into 0 — i.e. /auth/me told the console that an Enterprise account
                      -- (measured LIVE 2026-10-18: 25 such rows on that tier) had all_mechanics,
                      -- custom_domains, white_label, widget_embed, delivery_webhook ... switched
                      -- OFF while `tier_features.enabled` said otherwise. `1` is the map's "yes";
                      -- a numeric feature keeps its own limit (-1 = unlimited), and a feature that
                      -- is not enabled is still absent from the map entirely.
                      SELECT jsonb_object_agg(f.key, CASE WHEN tf.limit_value IS NULL THEN 1 ELSE tf.limit_value END)
                      FROM tier_features tf
                      JOIN features f ON f.id = tf.feature_id
                      WHERE tf.tier_id = t.id AND tf.enabled
                  ), '{}'::jsonb) as features,
                  t.max_campaigns,
                  t.max_entries_per_month,
                  COALESCE(t.price_monthly::float8, 0.0) as price_monthly,
                  COALESCE(t.price_annual::float8, 0.0) as price_annual
           FROM accounts a
           LEFT JOIN plan_tiers t ON a.plan_tier_id = t.id
           WHERE a.id = $1"#,
    )
    .bind(account_uuid)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let name: Option<String> = row.get("name");
    let email: String = row.get("email");
    let role: String = row.get("role");
    let plan_name: Option<String> = row.get("plan_name");
    // The account's own profile fields (migration zz_is12). `avatar_url` is DERIVED from the
    // `user_avatars` row's `updated_at` (chosen by the subquery in the SELECT above) instead of
    // being stored on `accounts`: with no stored URL there is no second writer that can leave a
    // reference to a picture that has since been replaced, and the `?v=` stamp rolls the browser
    // and CDN cache the moment a new picture lands.
    let username: Option<String> = row.get("username");
    let company: Option<String> = row.get("company");
    let avatar_at: Option<chrono::DateTime<chrono::Utc>> = row.get("avatar_updated_at");
    let avatar_url = avatar_at
        .map(|t| {
            format!(
                "/api/v1/auth/avatar/{}?v={}",
                user.account_id,
                t.timestamp()
            )
        })
        .unwrap_or_default();
    // The tier, as the console paints it. A tier-less account reads "Free" rather than an empty
    // badge — and never the raw role word ("User"), which is the stray label this card removes.
    let plan_label = plan_name.clone().unwrap_or_else(|| "Free".to_string());
    let plan_slug: Option<String> = row.get("plan_slug");
    let features: serde_json::Value = row.get("features");
    let plan_tier_id: Option<uuid::Uuid> = row.get("plan_tier_id");
    let max_campaigns: Option<i32> = row.get("max_campaigns");
    let max_entries: Option<i32> = row.get("max_entries_per_month");
    // The account's OWN tier price (plan_tiers), so the tenant view never has to price its own
    // plan by slug-joining the public `plans` catalogue (kanban t_1a29087b). Same decode
    // convention as tier_handler.rs / admin_handler.rs: numeric -> float8 in SQL, f64 here.
    let price_monthly: f64 = row.get("price_monthly");
    let price_annual: f64 = row.get("price_annual");

    // Fetch account industries
    let industries: Vec<Value> = sqlx::query(
        r#"SELECT i.id, i.name, i.slug, i.icon, ai.is_primary
           FROM account_industries ai
           JOIN industries i ON i.id = ai.industry_id
           WHERE ai.account_id = $1
           ORDER BY ai.is_primary DESC, i.sort_order"#,
    )
    .bind(account_uuid)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<String, _>("name"),
            "slug": r.get::<String, _>("slug"),
            "icon": r.get::<Option<String>, _>("icon"),
            "is_primary": r.get::<bool, _>("is_primary"),
        })
    })
    .collect();

    // Industry limit — ONE definition of the entitlement, shared with the PUT enforcement in
    // update_profile: features::industry_limit reads `tier_features.limit_value` for this account's
    // plan tier. Deliberately not `plans.features`: that is a jsonb ARRAY on the marketing/checkout
    // table and only ever matched by id coincidence, which pinned this field at 1 for every account
    // (kanban t_0961f382). No row for the key => the documented default 1.
    let industry_limit: i32 =
        crate::features::industry_limit(&state.db, account_uuid).await? as i32;

    Ok(Json(json!({
        "user": {
            "id": user.account_id,
            "email": email,
            "name": name.unwrap_or_default(),
            "role": role,
            // The account's own profile (kanban t_4fcbe895). `plan_name` is the REAL tier; the
            // console paints this and never the raw role word.
            "username": username.clone().unwrap_or_default(),
            "company": company.clone().unwrap_or_default(),
            "avatar_url": avatar_url,
            "plan_name": plan_label,
            "plan_tier_id": plan_tier_id,
            "plan": {
                "name": plan_name,
                "slug": plan_slug,
                "features": features,
                "max_campaigns": max_campaigns.unwrap_or(0),
                "max_entries_per_month": max_entries.unwrap_or(0),
                "price_monthly": price_monthly,
                "price_annual": price_annual,
            },
            "industries": industries,
            "industry_limit": industry_limit,
            "impersonating": user.impersonating
        },
        // Flat twin of `user.plan_name`: the fleet contract names `plan_name` as a field of this
        // response, and a client must not have to know this app nests the account under `user`.
        // Both placements are the SAME computed string (`plan_label`), so they cannot disagree.
        "plan_name": plan_label
    })))
}

/// PUT /api/v1/auth/profile
pub async fn update_profile(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateProfileInput>,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    // A blank NAME would leave the console header and every "signed in as" line empty, so it is
    // refused rather than stored. `company` and `username` may legitimately be cleared, so an empty
    // value is written for them (COALESCE only keeps a field the caller did not send at all).
    //
    // LENGTH, before the write: `accounts.company` and `accounts.username` are `varchar(255)` and
    // `accounts.name` is TEXT, and none of the three was checked — so a 300-character company or
    // username reached the UPDATE, Postgres refused it with 22001, and the caller got a bare
    // `500 {"code":500,"error":"Internal server error"}` while a 5000-character name stored
    // silently. Measured LIVE 2026-10-16 (company=300 -> 500, username=300 -> 500, name=5000 -> 200
    // stored); see `security::display_name`. All three are now refused at the door with the field
    // and the cap named, so the operator's screen shows something actionable.
    let name = match body.name.as_deref() {
        Some(raw) => {
            Some(crate::security::display_name::normalize_name(raw).map_err(AppError::BadRequest)?)
        }
        None => None,
    };
    crate::security::display_name::check_len(
        body.company.as_deref().unwrap_or(""),
        crate::security::display_name::MAX_PROFILE_FIELD_CHARS,
        "company",
    )
    .map_err(AppError::BadRequest)?;
    crate::security::display_name::check_len(
        body.username.as_deref().unwrap_or(""),
        crate::security::display_name::MAX_PROFILE_FIELD_CHARS,
        "username",
    )
    .map_err(AppError::BadRequest)?;

    // ONE statement, so a request carrying name + company + username either lands whole or not at
    // all; `updated_at` is stamped here (nothing else in this app writes these columns).
    sqlx::query(
        r#"-- The fleet profile contract (kanban t_4fcbe895): name is required non-blank above,
           -- company/username are optional and clearable. `updated_at` is nullable-safe:
           -- accounts.updated_at is NOT NULL with a default, so NOW() is always writable.
           UPDATE accounts SET name = COALESCE($1, name),
                               username = COALESCE($2, username),
                               company = COALESCE($3, company),
                               updated_at = NOW()
           WHERE id = $4"#,
    )
    .bind(name.as_deref())
    .bind(body.username.as_deref().map(str::trim))
    .bind(body.company.as_deref().map(str::trim))
    .bind(account_uuid)
    .execute(&state.db)
    .await?;

    // Industry change: set as primary, swapping out previous primary if at limit
    if let Some(ref industry_slug) = body.industry_slug {
        // Get the industry ID
        let industry_id: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM industries WHERE slug = $1 AND is_active = true")
                .bind(industry_slug)
                .fetch_optional(&state.db)
                .await?
                .flatten();

        if let Some(ind_id) = industry_id {
            // Check if already assigned — if so just make it primary
            let already_assigned: bool = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM account_industries WHERE account_id = $1 AND industry_id = $2"
            )
            .bind(account_uuid)
            .bind(ind_id)
            .fetch_one(&state.db)
            .await?
            > 0;

            if !already_assigned {
                // Plan limit, resolved from the entitlement model (`tier_features.limit_value` for
                // the account's plan tier) by `features::industry_limit`. This statement used to
                // join `plans` — the marketing/checkout table, whose `features` column is a jsonb
                // ARRAY and whose ids only coincide with `plan_tiers` for one row — so the cap was
                // silently ALWAYS 1 (never a 500, which is why two sweeps missed it; kanban
                // t_0961f382). Semantics of the returned value: -1 unlimited, 0 not available on
                // this plan, N > 0 cap N, and 1 when the tier does not configure the key.
                let industry_limit =
                    crate::features::industry_limit(&state.db, account_uuid).await?;

                let current_count = sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT COUNT(*) FROM account_industries WHERE account_id = $1",
                )
                .bind(account_uuid)
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);

                if industry_limit == 0 {
                    return Err(AppError::UpgradeRequired(
                        "Industry dashboards are not available on your current plan. Upgrade to add one."
                            .to_string(),
                    ));
                }

                if industry_limit > 0 && current_count >= industry_limit {
                    return Err(AppError::BadRequest(format!(
                        "Plan limit reached: maximum {} industries. Upgrade your plan to add more.",
                        industry_limit
                    )));
                }

                sqlx::query(
                    r#"INSERT INTO account_industries (account_id, industry_id, is_primary)
                       VALUES ($1, $2, true)
                       ON CONFLICT (account_id, industry_id) DO UPDATE SET is_primary = true"#,
                )
                .bind(account_uuid)
                .bind(ind_id)
                .execute(&state.db)
                .await?;
            } else {
                // Already assigned — just set as primary
                sqlx::query(
                    "UPDATE account_industries SET is_primary = true WHERE account_id = $1 AND industry_id = $2"
                )
                .bind(account_uuid)
                .bind(ind_id)
                .execute(&state.db)
                .await?;
            }

            // Unset other primaries
            sqlx::query(
                r#"UPDATE account_industries SET is_primary = false
                   WHERE account_id = $1 AND industry_id != $2 AND is_primary = true"#,
            )
            .bind(account_uuid)
            .bind(ind_id)
            .execute(&state.db)
            .await?;
        }
    }

    // Return full profile (same shape as /me for consistency)
    let row = sqlx::query(
        r#"-- The tier shape lives in `plan_tiers`: accounts.plan_tier_id has an FK to plan_tiers(id), and
           -- features.rs / access::feature_gate resolve max_campaigns, max_entries_per_month and the
           -- enabled feature keys off that same table + tier_features. `plans` is the marketing/checkout
           -- table and never had max_campaigns / max_entries_per_month / an enabled-key JSONB, so this
           -- statement 500'd on EVERY call for EVERY account (plain-statement column drift, kanban
           -- t_c7ba0473; same class as t_cf7469bb, which missed this one because it is a JOIN).
           -- GET /api/v1/auth/me and PUT /api/v1/auth/profile share this statement verbatim.
           SELECT a.name, a.email, a.role, a.plan_tier_id,
                  a.username, a.company,
                  (SELECT ua.updated_at FROM user_avatars ua WHERE ua.user_id = a.id) AS avatar_updated_at,
                  t.name as plan_name,
                  t.slug as plan_slug,
                  COALESCE((
                      -- An included BOOLEAN feature carries NO limit_value (the row is
                      -- enabled=true, limit_value NULL), so the old COALESCE turned every one of
                      -- them into 0 — i.e. /auth/me told the console that an Enterprise account
                      -- (measured LIVE 2026-10-18: 25 such rows on that tier) had all_mechanics,
                      -- custom_domains, white_label, widget_embed, delivery_webhook ... switched
                      -- OFF while `tier_features.enabled` said otherwise. `1` is the map's "yes";
                      -- a numeric feature keeps its own limit (-1 = unlimited), and a feature that
                      -- is not enabled is still absent from the map entirely.
                      SELECT jsonb_object_agg(f.key, CASE WHEN tf.limit_value IS NULL THEN 1 ELSE tf.limit_value END)
                      FROM tier_features tf
                      JOIN features f ON f.id = tf.feature_id
                      WHERE tf.tier_id = t.id AND tf.enabled
                  ), '{}'::jsonb) as features,
                  t.max_campaigns,
                  t.max_entries_per_month,
                  COALESCE(t.price_monthly::float8, 0.0) as price_monthly,
                  COALESCE(t.price_annual::float8, 0.0) as price_annual
           FROM accounts a
           LEFT JOIN plan_tiers t ON a.plan_tier_id = t.id
           WHERE a.id = $1"#,
    )
    .bind(account_uuid)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let name: Option<String> = row.get("name");
    let email: String = row.get("email");
    let role: String = row.get("role");
    let features: serde_json::Value = row.get("features");

    let industries: Vec<Value> = sqlx::query(
        r#"SELECT i.id, i.name, i.slug, i.icon, ai.is_primary
           FROM account_industries ai
           JOIN industries i ON i.id = ai.industry_id
           WHERE ai.account_id = $1
           ORDER BY ai.is_primary DESC, i.sort_order"#,
    )
    .bind(account_uuid)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<String, _>("name"),
            "slug": r.get::<String, _>("slug"),
            "icon": r.get::<Option<String>, _>("icon"),
            "is_primary": r.get::<bool, _>("is_primary"),
        })
    })
    .collect();

    // Same entitlement definition as me() and as the PUT enforcement above — see
    // features::industry_limit (kanban t_0961f382).
    let industry_limit: i32 =
        crate::features::industry_limit(&state.db, account_uuid).await? as i32;

    // Same profile fields as /me, from the same shared statement (see the note on that SELECT):
    // the console reads this response back into the form it just saved, so the two shapes must
    // agree or the form would repaint itself from a different source than the one it saved to.
    let username: Option<String> = row.get("username");
    let company: Option<String> = row.get("company");
    let avatar_at: Option<chrono::DateTime<chrono::Utc>> = row.get("avatar_updated_at");
    let avatar_url = avatar_at
        .map(|t| {
            format!(
                "/api/v1/auth/avatar/{}?v={}",
                user.account_id,
                t.timestamp()
            )
        })
        .unwrap_or_default();
    let plan_name: Option<String> = row.get("plan_name");
    let plan_label = plan_name.clone().unwrap_or_else(|| "Free".to_string());
    let plan_slug: Option<String> = row.get("plan_slug");
    let plan_tier_id: Option<uuid::Uuid> = row.get("plan_tier_id");
    let max_campaigns: Option<i32> = row.get("max_campaigns");
    let max_entries: Option<i32> = row.get("max_entries_per_month");
    // The account's OWN tier price (plan_tiers), so the tenant view never has to price its own
    // plan by slug-joining the public `plans` catalogue (kanban t_1a29087b). Same decode
    // convention as tier_handler.rs / admin_handler.rs: numeric -> float8 in SQL, f64 here.
    let price_monthly: f64 = row.get("price_monthly");
    let price_annual: f64 = row.get("price_annual");

    Ok(Json(json!({
        "user": {
            "id": user.account_id,
            "email": email,
            "name": name.unwrap_or_default(),
            "role": role,
            // The account's own profile (kanban t_4fcbe895). `plan_name` is the REAL tier; the
            // console paints this and never the raw role word.
            "username": username.clone().unwrap_or_default(),
            "company": company.clone().unwrap_or_default(),
            "avatar_url": avatar_url,
            "plan_name": plan_label,
            "plan_tier_id": plan_tier_id,
            "plan": {
                "name": plan_name,
                "slug": plan_slug,
                "features": features,
                "max_campaigns": max_campaigns.unwrap_or(0),
                "max_entries_per_month": max_entries.unwrap_or(0),
                "price_monthly": price_monthly,
                "price_annual": price_annual,
            },
            "industries": industries,
            "industry_limit": industry_limit,
        },
        // The fleet profile contract's own reply shape (kanban t_4fcbe895, matching the FunnelSwift
        // reference): the write reports `{"status":"ok"}`. The full user object rides along too,
        // because this app's console re-reads the row it just saved from this same response.
        "status": "ok"
    })))
}

/// PUT /api/v1/auth/password
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ChangePasswordInput>,
) -> Result<Json<Value>, AppError> {
    // The fleet profile contract's floor (kanban t_4fcbe895): the console refuses a short password
    // before sending, and this is the authoritative backstop for any other caller.
    if body.new_password.len() < 8 {
        return Err(AppError::BadRequest(
            "New password must be at least 8 characters".to_string(),
        ));
    }

    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    // Verify current password
    let row = sqlx::query(r#"SELECT password_hash FROM accounts WHERE id = $1"#)
        .bind(account_uuid)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let password_hash: Option<String> = row.get("password_hash");

    match password_hash {
        Some(ref hash) => {
            if !verify_password(&body.current_password, hash) {
                // 401, not 403: the fleet profile contract names this refusal, and a client cannot
                // act on "forbidden" (it reads as "you may not use this screen") whereas a 401 on a
                // credential check means exactly "that password is wrong, try again" (kanban
                // t_4fcbe895, matching the FunnelSwift reference). The caller IS authenticated —
                // their bearer token got them this far — so the only secret in question is the one
                // they just typed.
                return Err(AppError::Unauthorized(
                    "Current password is incorrect".to_string(),
                ));
            }
        }
        None => {
            return Err(AppError::Unauthorized(
                "No password set for this account".to_string(),
            ));
        }
    }

    let new_hash = hash_password(&body.new_password)?;

    // SESSION REVOCATION (2026-10-20): changing the password also ends every JWT issued BEFORE it.
    // Measured before this: a token minted before the change kept answering 200 afterwards, so the
    // one action a user takes when they believe their session is stolen revoked nothing.
    sqlx::query(
        "UPDATE accounts SET password_hash = $1, sessions_valid_from = NOW() WHERE id = $2",
    )
    .bind(&new_hash)
    .bind(account_uuid)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({ "status": "password_updated" })))
}

/// `POST /api/v1/auth/avatar` — store the caller's own profile picture.
///
/// Storage decision (kanban t_4fcbe895). This container binds only its release binary and
/// `migrations/` (`docker inspect incentiveswift`), so a file written at run time lives inside the
/// container and dies with the next recreate — and no host webroot could serve it either. The bytes
/// are therefore held in `user_avatars` (one row per account, migration zz_is12) and streamed back
/// by [`get_avatar`]. The multipart read, the size cap and the magic-byte sniff are
/// `crate::image_store`'s — the ONE accept/store/serve path, shared with the account's own email
/// logo (kanban t_feab8aff).
pub async fn upload_avatar(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    request: Request,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::Unauthorized("Invalid account id".to_string()))?;

    // The ONE shared image path: a multipart form OR a raw image body, the 2 MB envelope cap, and
    // the magic-byte sniff to PNG / JPEG / GIF / WebP. Every refusal is a 4xx.
    let (content_type, bytes) =
        crate::image_store::read_request_image(&state, &headers, request).await?;

    sqlx::query(
        r#"-- One picture per account: a later upload REPLACES the bytes in place, so the row's
           -- updated_at (which /auth/me turns into the `?v=` cache stamp) always tracks the bytes.
           INSERT INTO user_avatars (user_id, content_type, bytes, updated_at)
           VALUES ($1, $2, $3, NOW())
           ON CONFLICT (user_id) DO UPDATE
             SET content_type = EXCLUDED.content_type,
                 bytes = EXCLUDED.bytes,
                 updated_at = NOW()"#,
    )
    .bind(account_uuid)
    .bind(&content_type)
    .bind(&bytes)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // NO URL is stored anywhere: /auth/me derives it from this row, so a replaced picture can never
    // leave a stale reference behind. The URL is still returned here (`?v=` stamped now) so the
    // console repaints its header without a second round trip.
    let avatar_url = format!(
        "/api/v1/auth/avatar/{}?v={}",
        user.account_id,
        chrono::Utc::now().timestamp()
    );

    Ok(Json(json!({
        "status": "ok",
        "avatar_url": avatar_url,
        "content_type": content_type,
    })))
}

/// `DELETE /api/v1/auth/avatar` — remove the caller's OWN profile picture.
///
/// The upload twin only ever REPLACES the row (`ON CONFLICT (user_id) DO UPDATE`), so before this
/// route existed a picture could be swapped for another one but never taken down: an operator who
/// wanted their initials back had no way to ask, and the console could only offer "Upload picture".
/// The account's own email-branding logo has had its own DELETE since it shipped
/// (`DELETE /api/v1/settings/branding/logo`), so this closes the same gap on the profile.
///
/// It writes the SESSION's own account — there is no path id to spoof, the same rule `upload_avatar`
/// follows — and it is idempotent: `removed` reports whether a row was actually there, so calling it
/// on an account with no picture answers 200 rather than inventing a 404 for a state that is already
/// what the caller asked for. `avatar_url` comes back empty so the caller can repaint from the
/// response instead of re-reading `/auth/me`.
///
/// ONE row carries the picture on this app: the BYTES in `user_avatars`. There is no pointer column
/// to clear alongside them — `accounts` has no `avatar_url` (migration zz_is12) and `/auth/me`
/// DERIVES the URL from this row's `updated_at`, so removing the row IS what clears the URL and
/// makes the console fall back to its initial-letter placeholder. (Contrast ADASwift, whose
/// `users.avatar_url` pointer has to be nulled in the same transaction or `/auth/me` keeps handing
/// the console a URL that 404s.)
pub async fn delete_avatar(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::Unauthorized("Invalid account id".to_string()))?;

    let removed = sqlx::query("DELETE FROM user_avatars WHERE user_id = $1")
        .bind(account_uuid)
        .execute(&state.db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .rows_affected();

    Ok(Json(json!({
        "status": "ok",
        "removed": removed,
        "avatar_url": "",
    })))
}

/// `GET /api/v1/auth/avatar/:user_id` — stream a stored profile picture back.
///
/// Deliberately reachable with NO credential (see `security::route_policy::PUBLIC_ROUTES`): an HTML
/// `<img src>` cannot carry the bearer token, and a profile picture is not a secret. The route can
/// only ever return the picture one account uploaded — the id is an unguessable uuid, the stored
/// `content_type` was sniffed from the bytes at upload time, and an account with no picture answers
/// 404 so the console falls back to its initial-letter placeholder.
pub async fn get_avatar(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Response, AppError> {
    let uid = Uuid::parse_str(&user_id)
        .map_err(|_| AppError::NotFound("No such profile picture".to_string()))?;

    let row: Option<(String, Vec<u8>)> =
        sqlx::query_as("SELECT content_type, bytes FROM user_avatars WHERE user_id = $1")
            .bind(uid)
            .fetch_optional(&state.db)
            .await?;

    let (content_type, bytes) =
        row.ok_or_else(|| AppError::NotFound("No such profile picture".to_string()))?;

    // The shared response arm (crate::image_store): the sniffed type pinned, `nosniff`, and caching
    // that never invites a shared cache to keep one customer's picture.
    crate::image_store::image_response(content_type, bytes)
}

/// POST /api/v1/auth/forgot-password
pub async fn forgot_password(
    State(state): State<AppState>,
    Json(body): Json<ForgotPasswordInput>,
) -> Result<Json<Value>, AppError> {
    // The SAME boundary rule as `register`, from the same function: an address that could never
    // receive the reset mail is refused with the same 4xx/field shape instead of silently reporting
    // "if the email exists\u2026" for an address that cannot exist as a mailbox. The reply stays
    // unconditional for every well-formed address, so it still leaks nothing about accounts.
    let email = email_addr::normalize(&body.email).map_err(AppError::BadRequest)?;

    // Check if account exists. The NAME travels with the id so the reset mail can greet the
    // customer by name instead of the hard-coded "Hi there," it used to send (fixed 2026-10-13,
    // cron `default`) — the app already knows who they are.
    let account: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, coalesce(name, '') FROM accounts WHERE lower(email) = $1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;

    // Always return success to prevent email enumeration
    if account.is_none() {
        return Ok(Json(json!({
            "status": "ok",
            "message": "If the email exists, a reset link has been sent",
        })));
    }

    let (account_id, account_name) = account.unwrap();
    let token = Uuid::new_v4().to_string();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);

    sqlx::query(
        r#"INSERT INTO password_resets (account_id, token, expires_at)
           VALUES ($1, $2, $3)"#,
    )
    .bind(account_id)
    .bind(&token)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    // NEVER log the raw token. It is a bearer credential: whoever reads the log
    // can POST it to /api/v1/auth/reset-password and take the account over, so
    // the fallback log line was itself the vulnerability. Log a short SHA-256
    // hint instead — enough to correlate a support request with a reset row,
    // useless for redeeming one.
    let token_hint = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(token.as_bytes());
        hex::encode(&digest[..4])
    };
    tracing::info!(
        "Password reset token issued for {} (hint {}, expires at {})",
        email,
        token_hint,
        expires_at
    );

    // Attempt to send the email via configured provider
    let email_sent =
        crate::email::send_reset_email(&state.db, account_id, &email, &token, &account_name).await;
    if let Err(e) = email_sent {
        tracing::warn!("Failed to send password reset email: {}", e);
    }

    Ok(Json(json!({
        "status": "ok",
        "message": "If the email exists, a reset link has been sent",
    })))
}

/// POST /api/v1/auth/reset-password
pub async fn reset_password(
    State(state): State<AppState>,
    Json(body): Json<ResetPasswordInput>,
) -> Result<Json<Value>, AppError> {
    // Look up the reset token
    let row = sqlx::query(
        r#"SELECT account_id, expires_at, used
           FROM password_resets WHERE token = $1"#,
    )
    .bind(&body.token)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Invalid or expired reset token".to_string()))?;

    let account_id: Uuid = row.get("account_id");
    let expires_at: chrono::DateTime<chrono::Utc> = row.get("expires_at");
    let used: bool = row.get("used");

    if used {
        return Err(AppError::BadRequest(
            "Reset token has already been used".to_string(),
        ));
    }

    if chrono::Utc::now() > expires_at {
        return Err(AppError::BadRequest("Reset token has expired".to_string()));
    }

    let new_hash = hash_password(&body.new_password)?;

    // Update account password and mark token as used. SESSION REVOCATION (2026-10-20): the reset
    // also ends every JWT issued BEFORE it — measured before this: a token minted before a reset
    // still answered 200 from `/auth/me` afterwards, which is the whole point of resetting.
    sqlx::query(
        "UPDATE accounts SET password_hash = $1, sessions_valid_from = NOW() WHERE id = $2",
    )
    .bind(&new_hash)
    .bind(account_id)
    .execute(&state.db)
    .await?;

    sqlx::query("UPDATE password_resets SET used = true WHERE token = $1")
        .bind(&body.token)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "status": "password_reset" })))
}

pub async fn get_usage(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let usage = crate::features::get_usage_json(&state.db, &user.account_id).await;
    Ok(Json(usage))
}

/// Sign-in address change — REQUEST body (kanban t_6b2702e3).
#[derive(Deserialize)]
pub struct EmailChangeInput {
    pub new_email: String,
}

/// Sign-in address change — CONFIRM body. The token IS the credential the customer just received.
#[derive(Deserialize)]
pub struct ConfirmEmailChangeInput {
    pub token: String,
}

/// `POST /api/v1/auth/email-change` — ask to move the caller's OWN sign-in address.
///
/// Writes a PENDING row and mails a confirmation link to the NEW address; `accounts.email` does not
/// move until that link is opened. Two properties make it safe to expose on a profile screen:
///
///  * it writes the SESSION's own row (no path id to spoof, the same rule `upload_avatar` follows),
///    so one customer can never move another's address;
///  * the address only becomes the login identity once the person who controls that mailbox proves
///    it, which is what stops a typo (or a hostile edit) from locking someone out of their account.
///
/// Uniqueness is checked here so a clash reads as a sentence rather than as a 500 out of the
/// `lower(email)` collision, and again at confirm time because the address can be taken in between.
pub async fn request_email_change(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<EmailChangeInput>,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::Unauthorized("Invalid account id".to_string()))?;
    // Normalises (trim + lowercase) as well as validates — the stored value must be the value that
    // can receive the link, the same rule `register` follows.
    let new_email = email_addr::normalize(&body.new_email).map_err(AppError::BadRequest)?;

    let current: Option<(String, String)> =
        sqlx::query_as("SELECT email, COALESCE(name, '') FROM accounts WHERE id = $1")
            .bind(account_uuid)
            .fetch_optional(&state.db)
            .await?;
    let (current_email, name) =
        current.ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    if current_email.eq_ignore_ascii_case(&new_email) {
        return Err(AppError::BadRequest(
            "That is already your sign-in address".to_string(),
        ));
    }

    let taken: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = $1 AND id <> $2 LIMIT 1")
            .bind(&new_email)
            .bind(account_uuid)
            .fetch_optional(&state.db)
            .await?;
    if taken.is_some() {
        return Err(AppError::BadRequest(
            "That address is already used by another account".to_string(),
        ));
    }

    let token = Uuid::new_v4().to_string();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(2);

    // One live request per account: retire the previous one before inserting the new one, so a
    // customer who mistyped once cannot redeem the older link afterwards.
    sqlx::query("UPDATE email_changes SET used = true WHERE account_id = $1 AND used = false")
        .bind(account_uuid)
        .execute(&state.db)
        .await
        .ok();

    sqlx::query(
        "INSERT INTO email_changes (account_id, new_email, token, expires_at)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(account_uuid)
    .bind(&new_email)
    .bind(&token)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    let app_url = crate::email::APP_URL;
    let link = format!("{app_url}/confirm-email-change?token={token}");
    let greeting = if name.trim().is_empty() {
        "there"
    } else {
        name.trim()
    };
    let vars = json!({
        "name": greeting,
        "new_email": new_email,
        "link": link,
        "token": token,
        "app_url": app_url,
    });
    if let Err(e) = crate::email::send_template_email(
        &state.db,
        Some(account_uuid),
        &new_email,
        "email_change",
        &vars,
    )
    .await
    {
        tracing::error!(
            "email-change: the confirmation mail could not be handed to the provider for {}: {}",
            new_email,
            e
        );
        // No link reached the customer, so the pending row must not look redeemable.
        sqlx::query("UPDATE email_changes SET used = true WHERE token = $1")
            .bind(&token)
            .execute(&state.db)
            .await
            .ok();
        return Err(AppError::BadRequest(
            "We could not send the confirmation link, so nothing was changed. Try again in a moment."
                .to_string(),
        ));
    }

    // A reserved test address (RFC-2606 or a fleet harness domain) is withheld at the sender, so say
    // so plainly instead of telling the caller to check a mailbox that can never receive mail.
    let withheld = crate::email::recipient_is_undeliverable(&new_email);

    Ok(Json(json!({
        "status": "pending",
        "delivered": !withheld,
        "new_email": new_email,
        "message": if withheld {
            "This address is a reserved test address, so no mail was sent. Nothing has been changed."
        } else {
            "Check the new address for a confirmation link. Your sign-in email stays the same until you open it."
        },
    })))
}

/// `POST /api/v1/auth/email-change/confirm` — anonymous by necessity: the token IS the credential
/// the customer just received, and a link opened in a browser carries no session.
///
/// Reads ONE unexpired `email_changes` row, moves the caller's OWN `accounts.email` and nothing
/// else, and marks the row used so the link cannot be replayed.
pub async fn confirm_email_change(
    State(state): State<AppState>,
    Json(body): Json<ConfirmEmailChangeInput>,
) -> Result<Json<Value>, AppError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(AppError::BadRequest(
            "The confirmation link is missing its code".to_string(),
        ));
    }

    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT id, account_id, new_email FROM email_changes
         WHERE token = $1 AND used = false AND expires_at > NOW()",
    )
    .bind(&token)
    .fetch_optional(&state.db)
    .await?;
    let (change_id, account_id, new_email) = row.ok_or_else(|| {
        AppError::BadRequest("That confirmation link is invalid or has expired".to_string())
    })?;

    // The address may have been taken between the request and this confirmation.
    let taken: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = $1 AND id <> $2 LIMIT 1")
            .bind(&new_email)
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?;
    if taken.is_some() {
        return Err(AppError::BadRequest(
            "That address has since been used by another account, so nothing was changed"
                .to_string(),
        ));
    }

    let moved: Option<String> = sqlx::query_scalar(
        "UPDATE accounts SET email = $1, updated_at = NOW() WHERE id = $2 RETURNING email",
    )
    .bind(&new_email)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let moved = moved.ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    sqlx::query("UPDATE email_changes SET used = true WHERE id = $1")
        .bind(change_id)
        .execute(&state.db)
        .await?;

    tracing::info!(
        "email-change: account {} moved its sign-in address to {}",
        account_id,
        moved
    );

    Ok(Json(json!({
        "status": "ok",
        "email": moved,
        "message": "Your sign-in address has been changed. Use it the next time you sign in.",
    })))
}
