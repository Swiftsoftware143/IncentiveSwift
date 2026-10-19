use serde_json::json;

/// The product's own identity — what the `{{app_name}}` merge field carries.
pub(crate) const APP_NAME: &str = "IncentiveSwift";
/// Public app origin — what the `{{login_url}}` merge field carries, and the base of the
/// public campaign link the lifecycle sender binds as `{{share_link}}`/`{{referral_link}}`.
pub(crate) const APP_URL: &str = "https://app.incentiveswift.com";

/// Render a template string by replacing {{key}} placeholders with values from `vars`.
///
/// The vocabulary is double braces ONLY, which is what both sides of the admin surface
/// advertise (`GET /api/v1/email-templates/merge-fields` and the served console's merge-field
/// buttons). `{{#if key}}…{{/if}}` sections are resolved first, so a row can carry an optional
/// clause (kanban t_cdc24339). Anything left over is logged by name — kanban t_e43521d2: the
/// shipped `welcome` row spoke single braces, so `Welcome to {app_name}!` was mailed out verbatim
/// and nothing said so.
fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = crate::template_render::resolve_sections(template, vars);
    if let Some(obj) = vars.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = value.as_str().unwrap_or("");
            result = result.replace(&placeholder, replacement);
        }
    }
    crate::template_render::warn_unsubstituted(&result, "email template");
    result
}

/// Bind the two branding merge fields into a render's variable map (kanban t_feab8aff).
///
/// They are PER-ACCOUNT, so they cannot live in a `&'static` default map: the account's own values
/// when it has branding, the app's identity otherwise — so an admin-authored `{{brand_name}}` never
/// reaches a recipient as literal text. Every other key of the map is left alone.
fn bind_branding(vars: &mut serde_json::Value, branding: Option<&crate::branding::Branding>) {
    if !vars.is_object() {
        *vars = json!({});
    }
    let (name, logo) = match branding {
        Some(b) => (
            b.brand_name.clone(),
            b.resolve_logo_url(APP_URL).unwrap_or_default(),
        ),
        None => (APP_NAME.to_string(), String::new()),
    };
    if let Some(obj) = vars.as_object_mut() {
        obj.insert("brand_name".to_string(), json!(name));
        obj.insert("logo_url".to_string(), json!(logo));
    }
}

/// Put the account's branding at the TOP of a rendered message (kanban t_feab8aff).
///
/// The HTML part gains the header block (logo + name + colour rule); the text part gains the brand
/// name as a one-line header (a text part cannot carry an image). When the account HAS branding but
/// the message carries no HTML part — every inline fallback returns text only — one is built from
/// the ESCAPED text so a branded mail still shows the logo. A no-op for an account with no branding,
/// which is what makes the change additive: those renders are byte-identical to before.
fn with_branding_header(
    branding: Option<&crate::branding::Branding>,
    text: String,
    html: String,
) -> (String, String) {
    let Some(b) = branding else {
        return (text, html);
    };
    let logo = b.resolve_logo_url(APP_URL);
    let header = b.header_html(logo.as_deref());
    let html = if html.trim().is_empty() {
        format!(
            "{header}<div style=\"white-space:pre-wrap;font:14px/1.5 \
             -apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#111827\">{}</div>",
            crate::branding::escape_html(&text)
        )
    } else {
        format!("{header}{html}")
    };
    (format!("{}{}", b.text_header(), text), html)
}

/// Send a templated email using database-stored templates.
/// Falls back to old inline methods when no template found.
///
/// `account_id` is the account the mail is being sent FOR (`None` = no account in hand, which
/// degrades to the fleet-wide default). It is REQUIRED for tenant scope: this function used to
/// take no account at all and pick its row with `(aid IS NULL OR is_default = true)
/// ORDER BY is_default ASC, created_at DESC`, so one tenant's template became every tenant's
/// template — kanban t_0fb81177. The selection is now `delivery::sender::load_template_by_type`,
/// the app's single answer to "which template for this account" (see that function for the
/// measured shapes).
pub async fn send_template_email(
    pool: &sqlx::PgPool,
    account_id: Option<uuid::Uuid>,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<(), String> {
    let app_name = APP_NAME;
    let app_url = APP_URL;

    // Per-account email branding (kanban t_feab8aff). Loaded ONCE here, at the single funnel every
    // transactional type passes through, so a template added later inherits it for free. The two
    // merge fields are bound into the render map (per-account, so they cannot be static defaults)
    // and the header block is applied to the rendered parts below. An account with no branding
    // renders byte-identical mail to before this module existed.
    let branding = match account_id {
        Some(id) => crate::branding::load(pool, id).await,
        None => None,
    };
    let mut vars = vars.clone();
    // The template surface ADVERTISES `{{app_name}}` and `{{login_url}}` (the console's merge-field
    // list), but until now only the SUBJECT default and the inline fallback ever saw them — a
    // DB-ROW template that used them mailed the braces verbatim. Measured 2026-10-13 (cron
    // `default`): the `password_reset` row rendered "Open {{login_url}} and click …" to a live
    // customer. Bind them here, at the single funnel every transactional type passes through, the
    // same fix FunnelSwift carries (kanban t_2349e6ce). A caller that already set either key keeps
    // its own value.
    if !vars.is_object() {
        vars = json!({});
    }
    if let Some(obj) = vars.as_object_mut() {
        obj.entry("app_name".to_string())
            .or_insert_with(|| json!(APP_NAME));
        obj.entry("login_url".to_string())
            .or_insert_with(|| json!(APP_URL));
        obj.entry("app_url".to_string())
            .or_insert_with(|| json!(APP_URL));
    }
    bind_branding(&mut vars, branding.as_ref());
    let vars = &vars;

    let template = crate::delivery::sender::load_template_by_type(pool, account_id, template_type)
        .await
        .map_err(|e| format!("DB error loading template for '{template_type}': {e}"))?;

    let (subject, text_body, html_body) = match template {
        Some(t) => {
            let subject = render_template(
                &t.subject
                    .unwrap_or_else(|| get_default_subject(template_type, app_name)),
                vars,
            );
            let html_body = t
                .html_body
                .as_ref()
                .map(|h| render_template(h, vars))
                .unwrap_or_default();
            let text_body = render_template(&t.body.unwrap_or_default(), vars);
            let use_html = t.html_body.is_some();
            (
                subject,
                text_body,
                if use_html { html_body } else { String::new() },
            )
        }
        None => send_inline(to, template_type, vars, app_name, app_url).await?,
    };

    // The account's branding opens both parts of the message (a no-op without branding).
    let (text_body, html_body) = with_branding_header(branding.as_ref(), text_body, html_body);
    send_email_request(pool, to, &subject, &text_body, &html_body).await
}

fn get_default_subject(template_type: &str, app_name: &str) -> String {
    match template_type {
        // `welcome_credentials` is the checkout/onboarding flavour of the same mail (see
        // `send_welcome_email`), so it shares the subject.
        "welcome" | "welcome_credentials" => format!("Welcome to {}!", app_name),
        "purchase_confirmed" => "Payment Received — Thank You!".to_string(),
        "password_reset" => "Password Reset Request".to_string(),
        // The sign-in address change (kanban t_6b2702e3). Named for what the recipient must DO,
        // because the account has not moved yet — the link is the only thing that moves it.
        "email_change" => "Confirm your new sign-in email".to_string(),
        // The customer's own plan request / billing ticket was answered (2026-10-19). Named for
        // the thing the customer asked about, so it reads as an answer and not as a system notice.
        "plan_request_update" => format!("Update on your request — {app_name}"),
        _ => format!("{} Notification", app_name),
    }
}

async fn send_inline(
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
    app_name: &str,
    app_url: &str,
) -> Result<(String, String, String), String> {
    let name = vars.get("name").and_then(|v| v.as_str()).unwrap_or("there");
    let email = vars.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = vars.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let token = vars.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let plan_name_val = vars
        .get("plan_name")
        .and_then(|v| v.as_str())
        .unwrap_or("a plan");

    match template_type {
        // The inline safety net. `welcome_credentials` shares the body because it is the same
        // mail for the flow that MINTS the password; it is reached only when the DB row for
        // that type is absent (see `send_welcome_email`).
        "welcome" | "welcome_credentials" => {
            let body = format!(
                "Welcome to {}, {}!\n\nYour account has been created successfully.\n\nHere are your login credentials:\n\nEmail: {}\nPassword: {}\n\nLogin at: {}/login\n\nYou can now:\n- Create loyalty programs\n- Manage customer rewards\n- Track engagement metrics\n\nFor help, contact support@incentiveswift.com\n\nBest regards,\nThe {} Team",
                app_name, name, email, password, app_url, app_name
            );
            Ok((format!("Welcome to {}!", app_name), body, String::new()))
        }
        "purchase_confirmed" => {
            let body = format!(
                "Hi {},\n\nThank you for your purchase! Your payment for the {} plan has been received successfully.\n\nYou can access your dashboard at: {}/dashboard\n\nIf you have any questions, please contact support@incentiveswift.com\n\nBest regards,\nThe {} Team",
                name, plan_name_val, app_url, app_name
            );
            Ok((
                "Payment Received - Thank You!".to_string(),
                body,
                String::new(),
            ))
        }
        "password_reset" => {
            // The customer must be TOLD where to enter the code, and the mail must carry THIS app's
            // name. It used to hand over a 36-character code with no destination AND sign itself
            // "SwiftSoftware" — a company the customer has never heard of, on a mail that otherwise
            // belongs to IncentiveSwift (fixed 2026-10-13, cron `default`).
            // CORRECTED 2026-10-16 (cron `default`): the destination it was given was `/login`, and
            // that page has NO code field — its "Forgot password?" link goes to /forgot-password.html,
            // which asks for the ADDRESS and mails ANOTHER code. The page that accepts the code is
            // /reset-password (input id="t", prefilled from `?token=`). The 1-hour validity stated
            // here is correct for THIS app (auth_handler::forgot_password mints Duration::hours(1)),
            // so it is left alone.
            let body = format!(
                "Your password reset code is: {}\n\nOpen {}/reset-password?token={} to choose a new password - the code is filled in for you. If the form does not open, go to {}/reset-password and paste the code above.\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- The {} Team",
                token, app_url, token, app_url, app_name
            );
            Ok(("Password Reset Request".to_string(), body, String::new()))
        }
        // The inline safety net for the sign-in address change (kanban t_6b2702e3) — reached when
        // the account has no `email_change` template row. Two things the customer must be told, and
        // both were learned from the reset mail's own history: WHERE to click (a link, not a code),
        // and that NOTHING HAS CHANGED YET (the old address stays valid until the link is opened).
        "email_change" => {
            let new_email = vars
                .get("new_email")
                .and_then(|v| v.as_str())
                .unwrap_or("(the new address)");
            let link = vars.get("link").and_then(|v| v.as_str()).unwrap_or(app_url);
            let body = format!(
                "Hi {},\n\nWe received a request to change the email address you sign in to {} with, to:\n\n    {}\n\nTo confirm, open this link:\n\n    {}\n\nThe link works once and expires in 2 hours. Until you open it, nothing changes: your current address still works and your password stays the same. If you did not ask for this, ignore this email and nothing will happen.\n\n- The {} Team",
                name, app_name, new_email, link, app_name
            );
            Ok((
                "Confirm your new sign-in email".to_string(),
                body,
                String::new(),
            ))
        }
        // The inline safety net for the customer's own answered request (2026-10-19) — reached when
        // the account has no `plan_request_update` template row. It says the three things the
        // customer needs: WHICH request, its state now, and where to read the team's note again.
        "plan_request_update" => {
            let request = vars
                .get("request")
                .and_then(|v| v.as_str())
                .unwrap_or("your request");
            let status_word = vars
                .get("status_word")
                .and_then(|v| v.as_str())
                .unwrap_or("updated");
            let reply = vars.get("reply").and_then(|v| v.as_str()).unwrap_or("");
            let note = if reply.trim().is_empty() {
                String::new()
            } else {
                format!("\n\nThe team's note:\n\n{}\n", reply.trim())
            };
            let body = format!(
                "Hi {},\n\nHere is an update on \"{}\" — it is now: {}.{}\n\nYou can read this on your dashboard's Plan & billing screen at {}/dashboard/ at any time.\n\n- The {} Team",
                name, request, status_word, note, app_url, app_name
            );
            Ok((
                format!("Update on your request — {}", app_name),
                body,
                String::new(),
            ))
        }
        _ => {
            let body = format!("{} Notification:\n\n{}", app_name, vars);
            Ok((format!("{} Notification", app_name), body, String::new()))
        }
    }
}

/// Keep original functions for backward compatibility — now use DB templates
pub async fn send_welcome_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    password: &str,
) -> Result<(), String> {
    // The CREDENTIALS flavour of the welcome mail, and the reason it has its own
    // `template_type` (kanban t_e43521d2).
    //
    // The shared `welcome` row is selected by the SELF-SIGNUP producer
    // (`handlers::auth_handler::register`), where the account holder typed their own password
    // seconds earlier — so that row cannot honestly carry a `Password: {{password}}` line:
    // there is no password for the sender to bind, and mailing the one the user just chose is
    // exactly what the fleet's other credential mails avoid (WorkflowSwift
    // `checkout_handler.rs`, missedcallrespondr `checkout_handler.rs` and FunnelSwift
    // `admin_handler.rs` all bind a GENERATED password, never a user-chosen one).
    //
    // This producer is the Stripe/checkout onboarding: it MINTS the password
    // (`generate_temp_password()` in `billing::webhooks`), so it is the one flow that owes the
    // recipient the credential. `welcome_credentials` is seeded with that row by
    // `migrations/20260926_email_template_brace_vocabulary.sql`, which copies the original
    // `welcome` text — password line included — so no content was rewritten or lost, only
    // relocated to the template_type that can bind all of it.
    let vars = json!({
        "name": name,
        "email": to,
        "password": password,
        "app_name": APP_NAME,
        "login_url": APP_URL,
    });
    send_template_email(pool, Some(account_id), to, "welcome_credentials", &vars).await
}

pub async fn send_purchase_confirmed_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    plan_name: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": name,
        "plan_name": plan_name,
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, Some(account_id), to, "purchase_confirmed", &vars).await
}

pub async fn send_reset_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    token: &str,
    name: &str,
) -> Result<(), String> {
    let vars = json!({
        "token": token,
        // The customer's own name, not the "there" placeholder the reset mail used to greet every
        // recipient with (fixed 2026-10-13, cron `default`).
        "name": if name.trim().is_empty() { "there" } else { name.trim() },
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, Some(account_id), to, "password_reset", &vars).await
}

/// Tell a customer their own request moved (2026-10-19).
///
/// Until this existed the loop ended silently: a workspace asked for a paid plan from the Plan &
/// billing screen, the team answered (or moved the state) in the operator console, and the customer
/// was never told — the only way to learn the answer was to keep re-opening the dashboard. FunnelSwift
/// (`plan_request_update`) and WorkflowSwift ("the customer is told when their request is handled")
/// already mail on this event; this is the same mail for this app's own ticket queue.
///
/// `account_id` is the REQUESTER's account, so the mail renders that workspace's email branding.
pub async fn send_plan_request_update_email(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    to: &str,
    name: &str,
    request: &str,
    status_word: &str,
    reply: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": if name.trim().is_empty() { "there" } else { name.trim() },
        "request": request,
        "status_word": status_word.replace('_', " "),
        "reply": reply,
        "app_name": APP_NAME,
        "login_url": APP_URL,
        "app_url": APP_URL,
    });
    send_template_email(pool, Some(account_id), to, "plan_request_update", &vars).await
}

/// The operator's view of the LAST transactional send this installation attempted.
///
/// The send funnel already LOGS an outcome, but a log is not a panel. FunnelSwift records its own
/// last send (2026-10-14) and its comment names the siblings that do — ADASwift `email_send_state`,
/// MissedCall `email_last_send`, WorkflowSwift `last_send_ok`; IncentiveSwift recorded NOTHING, so
/// an operator could not tell from any screen whether a customer's credentials mail actually left.
/// Same key and shape as FunnelSwift (`admin_settings.email_last_send`), returned by
/// `GET /api/v1/admin/email-settings` and rendered on the console's Email Provider screen.
///
/// A WITHHELD send is recorded too (`ok: false` with the reason): the operator's question is "did the
/// customer get it", and a withheld message means no. Writing the record NEVER fails a send — an
/// error here is logged and dropped.
pub(crate) async fn record_last_send(
    pool: &sqlx::PgPool,
    to: &str,
    subject: &str,
    ok: bool,
    detail: &str,
) {
    write_panel(pool, to, subject, ok, detail).await;
    append_log(
        pool,
        to,
        subject,
        if ok { "sent" } else { "failed" },
        detail,
    )
    .await;
}

/// Append one row to the delivery history (`email_send_log`, migration zz_is15).
///
/// The history answers what the single slot cannot: it is append-only, so a credentials mail the
/// provider refused at signup is still readable after any later message succeeds. Writing it NEVER
/// fails a send — an error here is logged and dropped, exactly like the panel record below.
async fn append_log(pool: &sqlx::PgPool, to: &str, subject: &str, status: &str, detail: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO email_send_log (to_email, subject, status, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(to)
    .bind(subject)
    .bind(status)
    .bind(detail.chars().take(240).collect::<String>())
    .execute(pool)
    .await
    {
        tracing::warn!("could not append to email_send_log: {e}");
    }
}

/// Write the single-value record the Email Provider panel reads
/// (`admin_settings.email_last_send`) — "what happened to the most recent message?".
async fn write_panel(pool: &sqlx::PgPool, to: &str, subject: &str, ok: bool, detail: &str) {
    let payload = json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "ok": ok,
        "to": to,
        "subject": subject,
        "detail": detail.chars().take(240).collect::<String>(),
    });
    if let Err(e) = sqlx::query(
        "INSERT INTO admin_settings (key, value, description, updated_at) VALUES ('email_last_send', $1, 'The last transactional email this installation attempted to send (written by the send funnel)', NOW()) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()",
    )
    .bind(payload)
    .execute(pool)
    .await
    {
        tracing::warn!("could not record email_last_send: {e}");
    }
}

/// Record a send that was WITHHELD before any provider request (a reserved/harness recipient).
///
/// `withheld` is its own status, never `failed`: nothing was attempted, so nothing can be blamed on
/// the provider, and the panel must not tell the operator a customer was let down. The history row
/// is written on its own status; the single slot carries the same reason for the panel's one line.
pub(crate) async fn record_withheld(pool: &sqlx::PgPool, to: &str, subject: &str, reason: &str) {
    let detail = format!("withheld: {reason} (no send attempted)");
    append_log(pool, to, subject, "withheld", &detail).await;
    write_panel(pool, to, subject, false, &detail).await;
}

/// Whether a recipient can never receive mail (reserved RFC-2606 name, or a fleet harness domain).
/// Used to record an HONEST outcome for a send that was withheld before any provider request.
pub(crate) fn recipient_is_undeliverable(to: &str) -> bool {
    crate::security::probe_addr::harness_domain(to).is_some()
        || crate::security::email_addr::refuse_undeliverable_recipient(to).is_err()
}

/// Core email sender — the provider (`smtp | mailgun | sendgrid | sendiio`) and its credentials
/// come from the database (`admin_settings.email`, the fleet-wide system row). Nothing is read from
/// the process environment. The per-tenant override this used to fall back on is RETIRED (kanban
/// t_123b886b, see `email_provider`); a tenant's own server is the `smtp_*` family in
/// `delivery::sender`. This is also the arm an account with NO mail server of its own rides
/// (`delivery::sender::send_email`, kanban t_2e9117a5): one system-mail path, not two.
pub(crate) async fn send_email_request(
    pool: &sqlx::PgPool,
    to: &str,
    subject: &str,
    text_body: &str,
    html_body: &str,
) -> Result<(), String> {
    // Fleet harness addresses never reach a real relay (parity with FunnelSwift, kanban t_36b55ed2).
    // A probe that signs up with a fleet-dev domain (`swiftsoftware.dev/.net`) is created normally but
    // its mail is withheld: the address is routable, so a send can only land in a fleet mailbox or
    // bounce (measured 2026-10-09 on mail.incentiveswift.com: `accepted` then `bounced` 552), and every
    // such send burns a delivery on the domain's sending reputation. This function is the choke point
    // BOTH the template arm and the inline fallback pass through, so every transactional type is
    // covered. The RFC-2606 class is now ALSO suppressed (changed 2026-10-09 after measuring real provider attempts) (.local/.test/.invalid/example.*) is deliberately NOT suppressed —
    // content harnesses point the provider at a local sink and read the message off the wire, so
    // silencing it would delete proof.
    if let Some(domain) = crate::security::probe_addr::harness_domain(to) {
        tracing::info!(
            to = %to,
            domain = %domain,
            subject = %subject,
            "send suppressed: recipient is a fleet harness address (fleet-dev domain)"
        );
        record_withheld(pool, to, subject, "recipient is a fleet harness address").await;
        return Ok(());
    }
    let Some(cfg) = crate::email_provider::resolve(pool).await else {
        tracing::warn!(
            to = %to,
            subject = %subject,
            "email skipped — no email provider configured (Admin > Settings > Email Provider)"
        );
        record_last_send(pool, to, subject, false, "no email provider configured").await;
        return Err(
            "Email provider not configured. Set it in Admin > Settings > Email Provider."
                .to_string(),
        );
    };

    let html = if html_body.trim().is_empty() {
        None
    } else {
        Some(html_body)
    };

    // The RECIPIENT decides whether this was an attempt at all, not the shape of the outcome:
    // `deliver` refuses the RFC-2606 class with `Err("recipient-refused: …")` and the fleet-harness
    // class with `Ok(())`. Keying the withheld arm on `Ok` alone recorded a deliberately-withheld
    // message as a delivery FAILURE (measured 2026-10-18 through the panel's test-send), which is the
    // misreport the fleet rule forbids: nothing was attempted, so nothing may read as refused mail.
    let held = recipient_is_undeliverable(to);
    let outcome = crate::email_provider::deliver(pool, &cfg, to, subject, text_body, html).await;
    if held {
        record_withheld(pool, to, subject, "recipient cannot receive mail").await;
    } else {
        match &outcome {
            Ok(()) => record_last_send(pool, to, subject, true, "").await,
            Err(e) => {
                tracing::warn!(provider = %cfg.provider, to = %to, "email send failed: {e}");
                record_last_send(pool, to, subject, false, e).await;
            }
        }
    }
    // The OUTCOME is returned unchanged: `email_queue` deliberately treats a refused recipient as an
    // ordinary bounded failure (3 attempts, no provider request) and dead-letters it, which is the
    // surface that makes such a row visible. Only the RECORDING is honest now.
    outcome
}
