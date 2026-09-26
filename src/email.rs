use serde_json::json;

/// The product's own identity — what the `{{app_name}}` merge field carries.
const APP_NAME: &str = "IncentiveSwift";
/// Public app origin — what the `{{login_url}}` merge field carries.
const APP_URL: &str = "https://app.incentiveswift.com";

/// Render a template string by replacing {{key}} placeholders with values from `vars`.
///
/// The vocabulary is double braces ONLY, which is what both sides of the admin surface
/// advertise (`GET /api/v1/email-templates/merge-fields` and the served console's merge-field
/// buttons). Anything left over is logged by name — kanban t_e43521d2: the shipped `welcome`
/// row spoke single braces, so `Welcome to {app_name}!` was mailed out verbatim and nothing
/// said so.
fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = template.to_string();
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

/// Send a templated email using database-stored templates.
/// Falls back to old inline methods when no template found.
pub async fn send_template_email(
    pool: &sqlx::PgPool,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<(), String> {
    let app_name = "IncentiveSwift";
    let app_url = "https://app.incentiveswift.com";

    // Try to load template from DB.
    //
    // Gate rule 5a (class 5): the predicate used to be
    // `(aid = '<all-zeros uuid>' OR is_default = true)` — a hand-copied UUID written into the query
    // text (kanban t_017517a9). It named NO row, and that arm was dead: MEASURED on the live DB,
    // all 58 `email_templates` rows are `aid IS NULL AND is_default = true`, so the nil-uuid arm
    // matched 0 rows and `is_default` did all the work. The fleet-wide default is now named by the
    // shape the data actually has (`aid IS NULL`), which also makes the arm live: a fleet-wide row
    // that is NOT flagged default (the `is_default ASC` ordering below exists to prefer exactly
    // that row) is now found instead of silently skipped.
    let template = sqlx::query_as::<_, EmailTemplateRow>(
        r#"-- the template's HTML body IS the is_html flag: `email_templates` has no
           -- `is_html` column (plain-statement drift, kanban t_cf7469bb), and a row only
           -- sends HTML when it carries an `html_body`.
           SELECT id, name, subject, body, html_body, is_default
           FROM email_templates
           WHERE template_type = $1 AND (aid IS NULL OR is_default = true)
           ORDER BY is_default ASC, created_at DESC
           LIMIT 1"#,
    )
    .bind(template_type)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    match template {
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
            send_email_request(
                pool,
                to,
                &subject,
                &text_body,
                if use_html { &html_body } else { "" },
            )
            .await
        }
        None => send_inline(pool, to, template_type, vars, app_name, app_url).await,
    }
}

fn get_default_subject(template_type: &str, app_name: &str) -> String {
    match template_type {
        // `welcome_credentials` is the checkout/onboarding flavour of the same mail (see
        // `send_welcome_email`), so it shares the subject.
        "welcome" | "welcome_credentials" => format!("Welcome to {}!", app_name),
        "purchase_confirmed" => "Payment Received — Thank You!".to_string(),
        "password_reset" => "Password Reset Request".to_string(),
        _ => format!("{} Notification", app_name),
    }
}

async fn send_inline(
    pool: &sqlx::PgPool,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
    app_name: &str,
    app_url: &str,
) -> Result<(), String> {
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
            send_email_request(pool, to, &format!("Welcome to {}!", app_name), &body, "").await
        }
        "purchase_confirmed" => {
            let body = format!(
                "Hi {},\n\nThank you for your purchase! Your payment for the {} plan has been received successfully.\n\nYou can access your dashboard at: {}/dashboard\n\nIf you have any questions, please contact support@incentiveswift.com\n\nBest regards,\nThe {} Team",
                name, plan_name_val, app_url, app_name
            );
            send_email_request(pool, to, "Payment Received - Thank You!", &body, "").await
        }
        "password_reset" => {
            let body = format!(
                "Your password reset code is: {}\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- SwiftSoftware",
                token
            );
            send_email_request(pool, to, "Password Reset Request", &body, "").await
        }
        _ => {
            let body = format!("{} Notification:\n\n{}", app_name, vars);
            send_email_request(pool, to, &format!("{} Notification", app_name), &body, "").await
        }
    }
}

/// Keep original functions for backward compatibility — now use DB templates
pub async fn send_welcome_email(
    pool: &sqlx::PgPool,
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
    send_template_email(pool, to, "welcome_credentials", &vars).await
}

pub async fn send_purchase_confirmed_email(
    pool: &sqlx::PgPool,
    to: &str,
    name: &str,
    plan_name: &str,
) -> Result<(), String> {
    let vars = json!({
        "name": name,
        "plan_name": plan_name,
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, to, "purchase_confirmed", &vars).await
}

pub async fn send_reset_email(pool: &sqlx::PgPool, to: &str, token: &str) -> Result<(), String> {
    let vars = json!({
        "token": token,
        "name": "there",
        "app_url": "https://app.incentiveswift.com",
    });
    send_template_email(pool, to, "password_reset", &vars).await
}

/// Core email sender — the provider (`smtp | mailgun | sendgrid | sendiio`) and its
/// credentials come from the database (`admin_settings.email`, overridable per tenant in
/// `tenant_settings`). Nothing is read from the process environment.
async fn send_email_request(
    pool: &sqlx::PgPool,
    to: &str,
    subject: &str,
    text_body: &str,
    html_body: &str,
) -> Result<(), String> {
    let Some(cfg) = crate::email_provider::resolve(pool, None).await else {
        tracing::warn!(
            to = %to,
            subject = %subject,
            "email skipped — no email provider configured (Admin > Settings > Email Provider)"
        );
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

    crate::email_provider::deliver(&cfg, to, subject, text_body, html)
        .await
        .map_err(|e| {
            tracing::warn!(provider = %cfg.provider, to = %to, "email send failed: {e}");
            e
        })
}

#[derive(Debug, sqlx::FromRow)]
struct EmailTemplateRow {
    id: uuid::Uuid,
    name: String,
    subject: Option<String>,
    body: Option<String>,
    html_body: Option<String>,
    is_default: Option<bool>,
}
