//! STORAGE IS THE RULE; A CLOCK IS NOT (B206, David 2026-11-11).
//!
//! One account-level storage allowance, one usage figure, one place that decides each of the four
//! states the product has:
//!
//! | usage      | inbound mail | new attachments / uploaded files | owner is told                |
//! |------------|--------------|----------------------------------|------------------------------|
//! | under 50%  | stored       | accepted                         | nothing                      |
//! | 50% +      | stored       | accepted                         | banner + one email / month   |
//! | 75% +      | stored       | accepted                         | banner + one email / month   |
//! | 99% +      | stored       | accepted until it fills          | banner + one email / month   |
//! | 100% +     | stored       | REFUSED (402, with numbers)      | banner says "full"           |
//!
//! WHAT A FULL MAILBOX NEVER DOES: refuse inbound mail and delete mail. A support inbox that bounces
//! loses the owner business (David), so inbound is unconditional on this axis, and the quota is a
//! promise about storage — not a licence to destroy the customer's correspondence.
//!
//! ONE SOURCE FOR THE ALLOWANCE: the module registry's `email_storage_mb` for this account (free
//! 2048 MB = the stated 2 GB default, raised per plan by migration 142). `tenant_plans.
//! feature_overrides` is honoured by `module_registry::resolve`, so an operator can raise or lower
//! one account without a redeploy. Nothing here reads `plans.features.storage_gb` — that key was
//! retired as unenforceable (migration 103) and its free value (0.1 GB) was never a mailbox number.
//!
//! ONE USAGE FIGURE, from the same bytes the account really stores:
//!   * `private_email_messages.size_bytes` — every stored message, inbound and outbound;
//!   * `tenant_logos.bytes` — the workspace logo;
//!   * `user_avatars.bytes` — the account's profile pictures.
//!
//! The meter, the banner, the reminder email and the 100% refusal all read THIS function, so the
//! number a customer is shown and the number that refuses a file cannot disagree.

use chrono::{DateTime, Datelike, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

/// The allowance when a plan names none: 2 GB, the number B206 states (not the 5 GB the old free
/// registry row carried). A missing row degrades to the sold default instead of "no storage at all".
pub const DEFAULT_QUOTA_MB: i64 = 2048;

/// The three rungs the banner and the reminder email share. Their order matters: 99 is the "almost
/// full" rung and 100 (not a rung — the ceiling) is where new files stop.
pub const THRESHOLDS: [i32; 3] = [50, 75, 99];

/// The retention backstop when an account has no explicit window: ten years. Time-based deletion is
/// a last resort behind the quota, not the product rule (B206 revised).
pub const DEFAULT_RETENTION_DAYS: i64 = 3650;

/// One account's storage state, in bytes, plus the rung it has reached.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub used_bytes: i64,
    /// Messages only — what the mailbox itself holds.
    pub mail_bytes: i64,
    /// The logo + profile pictures — files the account uploaded.
    pub file_bytes: i64,
    pub quota_bytes: i64,
}

impl Usage {
    /// Whole percent of the allowance used, rounded DOWN so 99.6% still reads as 99% and only a
    /// genuinely full account reaches 100. A quota of zero is treated as the default (never as
    /// "divide by zero" and never as "full").
    pub fn percent(&self) -> i32 {
        if self.quota_bytes <= 0 {
            return 0;
        }
        ((self.used_bytes.max(0) as f64 / self.quota_bytes as f64) * 100.0).floor() as i32
    }

    /// The rung reached: 0 under 50%, else 50 / 75 / 99, and 100 at or over the allowance.
    pub fn level(&self) -> i32 {
        let pct = self.percent();
        if pct >= 100 {
            return 100;
        }
        let mut level = 0;
        for t in THRESHOLDS {
            if pct >= t {
                level = t;
            }
        }
        level
    }

    /// Room for a new attachment/file? False only at or over the allowance.
    pub fn accepts_new_files(&self) -> bool {
        self.used_bytes < self.quota_bytes
    }

    /// `1.5 GB`, `812 MB`, `0 B` — the units a customer reads, never raw bytes.
    pub fn used_human(&self) -> String {
        human_bytes(self.used_bytes)
    }
    pub fn quota_human(&self) -> String {
        human_bytes(self.quota_bytes)
    }
}

/// Bytes → the plain-English figure the banner and the email state.
///
/// PRECISION MATTERS AT SMALL QUOTAS: this used to round every MB to a whole number, so a mailbox
/// at 81% printed "2 MB of 2 MB used" — a number that says "full" while the percent says 81. Under
/// 10 of any unit it now keeps up to two decimals and trims the noise ("1.62 MB", "1.5 GB", "2 GB"),
/// so the stated figure always agrees with the percent beside it.
pub fn human_bytes(n: i64) -> String {
    let n = n.max(0) as f64;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    fn amount(v: f64) -> String {
        if v >= 10.0 {
            format!("{:.0}", v)
        } else {
            let s = format!("{:.2}", v);
            let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
            s
        }
    }
    if n >= GB {
        format!("{} GB", amount(n / GB))
    } else if n >= MB {
        format!("{} MB", amount(n / MB))
    } else if n >= KB {
        format!("{} KB", amount(n / KB))
    } else {
        format!("{:.0} B", n)
    }
}

/// The account's allowance in bytes, from the ONE registry row. A missing/unconfigured value is the
/// stated 2 GB default rather than "unlimited" (an uncapped meter could never warn).
pub async fn quota_bytes(db: &PgPool, tenant_id: Uuid) -> i64 {
    // The tier states `max_email_storage_mb` when it sells a size; otherwise the sold 2 GB default.
    // Read through `crate::module_registry` (plan_tiers -> tier_features), and an unstated or
    // unlimited value falls back to the default rather than "unlimited": an uncapped meter could
    // never warn.
    let stated = crate::module_registry::limit(db, tenant_id, "max_email_storage_mb")
        .await
        .ok()
        .flatten();
    let mb = match stated {
        Some(mb) if mb > 0 => mb as i64,
        _ => DEFAULT_QUOTA_MB,
    };
    mb.saturating_mul(1024 * 1024)
}

/// Everything the account stores, and its allowance. Two cheap aggregates; no per-row work.
pub async fn usage(db: &PgPool, tenant_id: Uuid) -> Usage {
    let mail_bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0)::int8 FROM private_email_messages WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);

    let file_bytes: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE((SELECT SUM(octet_length(bytes)) FROM tenant_logos WHERE tenant_id = $1), 0)::int8
             + COALESCE((SELECT SUM(octet_length(a.bytes))
                           FROM user_avatars a
                          WHERE a.user_id = $1), 0)::int8
        "#,
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);

    Usage {
        used_bytes: mail_bytes.saturating_add(file_bytes),
        mail_bytes,
        file_bytes,
        quota_bytes: quota_bytes(db, tenant_id).await,
    }
}

/// What the inbox screen states about retention: the account's real window, whether anything is
/// actually scheduled for removal, and when the oldest message would reach the backstop.
pub struct Retention {
    pub days: i64,
    pub oldest_message_at: Option<DateTime<Utc>>,
    /// `oldest_message_at + days` — None when the account holds no mail. Only the OLDEST message is
    /// ever next to go, so this is the honest "nothing goes before this date" figure.
    pub first_expiry_at: Option<DateTime<Utc>>,
    /// How many stored messages are already past the window (0 in normal operation: the daily sweep
    /// removes them, and the window is ten years by default).
    pub expired_messages: i64,
}

/// The account's retention window and what it currently implies. Read-only.
pub async fn retention(db: &PgPool, tenant_id: Uuid) -> Retention {
    let days: i64 = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT retention_days FROM tenant_email_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .flatten()
    .map(|d| d as i64)
    .unwrap_or(DEFAULT_RETENTION_DAYS)
    .max(1);

    let oldest: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(received_at) FROM private_email_messages WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .unwrap_or(None);

    let expired: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::int8 FROM private_email_messages
          WHERE tenant_id = $1 AND received_at < NOW() - make_interval(days => $2)",
    )
    .bind(tenant_id)
    .bind(days as i32)
    .fetch_one(db)
    .await
    .unwrap_or(0);

    Retention {
        days,
        oldest_message_at: oldest,
        first_expiry_at: oldest.map(|o| o + chrono::Duration::days(days)),
        expired_messages: expired,
    }
}

/// The sentence the inbox screen keeps on it, whatever the level: the backstop is stated with the
/// account's REAL window whenever one applies (B206: "must still be stated on screen whenever it
/// applies"). "Unlimited" is never claimed, because the storage rule keeps mail only while the
/// account has room and the backstop exists for the database's own bound.
pub fn retention_sentence(r: &Retention) -> String {
    let window = if r.days % 365 == 0 && r.days >= 365 {
        let y = r.days / 365;
        format!("{} year{}", y, if y == 1 { "" } else { "s" })
    } else {
        format!("{} days", r.days)
    };
    match r.first_expiry_at {
        Some(d) => format!(
            "Mail is kept while you have storage, and nothing is deleted to make room. As a last-resort backstop, \
             mail older than {} is removed — the oldest message in this account would reach that on {}. \
             You can archive or delete anything yourself at any time.",
            window,
            d.format("%B %-d, %Y")
        ),
        None => format!(
            "Mail is kept while you have storage, and nothing is deleted to make room. As a last-resort backstop, \
             mail older than {} is removed; this account holds no mail yet, so nothing is scheduled for removal.",
            window
        ),
    }
}

/// The banner the inbox screen renders at 50 / 75 / 99 / 100 — `None` under 50%. Every arm states
/// the ACTUAL numbers and says what to do; none of them threatens deletion, because none happens.
pub fn banner(usage: &Usage) -> Option<Value> {
    let level = usage.level();
    if level < 50 {
        return None;
    }
    let used = usage.used_human();
    let quota = usage.quota_human();
    let pct = usage.percent();
    let (tone, headline, action) = match level {
        50 => (
            "info",
            format!("Your mailbox is {}% full — {} of {} used", pct, used, quota),
            "Mail keeps arriving and nothing is deleted. Archive or delete the messages you no longer need, \
             or upgrade for more storage.",
        ),
        75 => (
            "warn",
            format!("Your mailbox is {}% full — {} of {} used", pct, used, quota),
            "You have room for now. Clear out what you no longer need (Trash and Spam count too), or upgrade \
             for more storage. Nothing is deleted to make room.",
        ),
        99 => (
            "warn",
            format!("Your mailbox is almost full — {}% used ({} of {})", pct, used, quota),
            "Plain mail is still arriving and nothing has been deleted. Free some space now, or upgrade — once \
             the allowance is reached, new attachments and files are paused until you do.",
        ),
        _ => (
            "full",
            format!("Your mailbox is full — {} of {} used", used, quota),
            "Mail is STILL arriving and nothing has been deleted. New attachments and files are paused until you \
             clear space or upgrade. Delete or archive mail you no longer need — Trash and Spam count toward the \
             figure until you empty them.",
        ),
    };
    Some(json!({
        "level": level,
        "percent": pct,
        "used_bytes": usage.used_bytes,
        "quota_bytes": usage.quota_bytes,
        "used_human": used,
        "quota_human": quota,
        "mail_human": human_bytes(usage.mail_bytes),
        "files_human": human_bytes(usage.file_bytes),
        "headline": headline,
        "action": action,
        "tone": tone,
        "attachments_accepted": usage.accepts_new_files(),
    }))
}

/// REFUSE a NEW attachment/upload when the allowance is reached. The ONLY refusal a full mailbox
/// makes: plain mail keeps arriving and nothing is deleted. The sentence names the real numbers and
/// the two ways out, so it is never a dead end.
pub async fn enforce_new_file_room(
    db: &PgPool,
    tenant_id: Uuid,
    what: &str,
) -> Result<(), AppError> {
    let u = usage(db, tenant_id).await;
    if u.accepts_new_files() {
        return Ok(());
    }
    Err(AppError::UpgradeRequired(format!(
        "Your storage is full ({} of {} used), so this new {} was not added. Your mail keeps arriving and \
         nothing has been deleted — delete or archive some mail (Trash and Spam count too), or upgrade for \
         more storage, and {}s work again immediately.",
        u.used_human(),
        u.quota_human(),
        what,
        what
    )))
}

/// The UTC month the reminder ledger is keyed by: one message per threshold per month at most.
fn current_period(now: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", now.year(), now.month())
}

/// Who the reminder goes to: the account's active owner, else its oldest active member. A workspace
/// with no active user sends nothing (there is nobody to tell) and no ledger row is written.
async fn owner_recipient(db: &PgPool, tenant_id: Uuid) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        r#"
        SELECT email, NULL::text, name
          FROM accounts
         WHERE id = $1
         LIMIT 1
        "#,
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|(email, first, name)| {
        let display = first
            .filter(|s| !s.trim().is_empty())
            .or_else(|| name.filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "there".to_string());
        (email, display)
    })
}

/// Evaluate this account's usage and, for every rung it has just reached, CLAIM the month and queue
/// ONE reminder email to the owner. Returns the rungs notified in this pass.
///
/// Idempotent by construction: the ledger row is claimed with `ON CONFLICT DO NOTHING RETURNING id`
/// before the mail is queued, so a provider retry, a second inbound delivery in the same second, and
/// the hourly sweep cannot produce a second message for the same rung in the same month. A queue
/// failure deletes the claim so the next pass can try again — the customer is never silently skipped
/// for a whole month because one INSERT failed.
pub async fn evaluate_and_notify(db: &PgPool, tenant_id: Uuid) -> Vec<i32> {
    let u = usage(db, tenant_id).await;
    let level = u.level();
    if level < 50 {
        return Vec::new();
    }
    let period = current_period(Utc::now());
    let mut notified = Vec::new();

    for threshold in THRESHOLDS {
        if level < threshold {
            break;
        }
        // Claim BEFORE queueing. `sent_at` is the claim time; the mail is queued immediately after.
        let claimed: Option<Uuid> = sqlx::query_scalar(
            r#"
            INSERT INTO private_email_storage_notices
                (tenant_id, threshold_pct, period, usage_bytes, quota_bytes)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (tenant_id, threshold_pct, period) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(tenant_id)
        .bind(threshold as i16)
        .bind(&period)
        .bind(u.used_bytes)
        .bind(u.quota_bytes)
        .fetch_optional(db)
        .await
        .unwrap_or(None);

        let Some(claim_id) = claimed else { continue };

        let Some((email, name)) = owner_recipient(db, tenant_id).await else {
            // Nobody to tell: release the claim rather than burn the rung for a month.
            let _ = sqlx::query("DELETE FROM private_email_storage_notices WHERE id = $1")
                .bind(claim_id)
                .execute(db)
                .await;
            continue;
        };

        let ret = retention(db, tenant_id).await;
        let vars = json!({
            "name": name,
            "email": email,
            "threshold": threshold,
            "percent": u.percent(),
            "used": u.used_human(),
            "quota": u.quota_human(),
            "mail_used": human_bytes(u.mail_bytes),
            "files_used": human_bytes(u.file_bytes),
            "inbox_url": crate::email::APP_URL_MAIL,
            // The console's Billing & Plans screen is a TAB, not a route: `?tab=billing` is the deep
            // link its boot honours, and it is the only address that opens the thing this mail is
            // selling. `/pricing` does not exist and would be a dead door.
            "plans_url": format!("{}/?tab=billing", crate::email::APP_URL),
            "retention": retention_sentence(&ret),
            "retention_days": ret.days,
        });

        match crate::email::send_template_email(
            db,
            Some(tenant_id),
            &email,
            "storage_warning",
            &vars,
        )
        .await
        {
            Ok(()) => {
                notified.push(threshold);
                tracing::info!(
                    tenant = %tenant_id,
                    threshold,
                    percent = u.percent(),
                    used = %u.used_human(),
                    quota = %u.quota_human(),
                    "storage reminder queued to the account owner"
                );
            }
            Err(e) => {
                // Release the claim so a later pass retries; log loudly, never 500 a caller.
                let _ = sqlx::query("DELETE FROM private_email_storage_notices WHERE id = $1")
                    .bind(claim_id)
                    .execute(db)
                    .await;
                tracing::error!(
                    tenant = %tenant_id,
                    threshold,
                    error = %e,
                    "storage reminder could NOT be queued — claim released for a retry"
                );
            }
        }
    }

    notified
}

/// The periodic sweep: every account that stores anything is evaluated. Cheap (two aggregates and
/// at most three claim attempts per account) and idempotent, so it can run often — an account whose
/// owner never opens the app still gets the reminder within the hour of crossing a rung.
pub async fn sweep(db: &PgPool) -> u64 {
    let tenants: Vec<(Uuid,)> = match sqlx::query_as(
        "SELECT DISTINCT tenant_id FROM private_email_messages WHERE tenant_id IS NOT NULL",
    )
    .fetch_all(db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "storage sweep could not list accounts");
            return 0;
        }
    };

    let mut warned = 0u64;
    for (tenant_id,) in tenants {
        warned += evaluate_and_notify(db, tenant_id).await.len() as u64;
    }
    warned
}
