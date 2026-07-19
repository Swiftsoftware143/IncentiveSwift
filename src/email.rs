use std::env;
use serde_json::json;

/// Render a template string by replacing {{key}} placeholders with values from `vars`.
fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = vars.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = value.as_str().unwrap_or("");
            result = result.replace(&placeholder, replacement);
        }
    }
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

    // Try to load template from DB
    let template = sqlx::query_as::<_, EmailTemplateRow>(
        r#"SELECT id, name, subject, body, html_body, is_html, is_default
           FROM email_templates
           WHERE template_type = $1 AND (aid = '00000000-0000-0000-0000-000000000000' OR is_default = true)
           ORDER BY is_default ASC, created_at DESC
           LIMIT 1"#
    )
    .bind(template_type)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    match template {
        Some(t) => {
            let subject = render_template(
                &t.subject.unwrap_or_else(|| get_default_subject(template_type, app_name)),
                vars,
            );
            let html_body = t.html_body.as_ref()
                .map(|h| render_template(h, vars))
                .unwrap_or_default();
            let text_body = render_template(&t.body.unwrap_or_default(), vars);
            let use_html = t.is_html.unwrap_or(true);
            send_email_request(to, &subject, &text_body, if use_html { &html_body } else { "" }).await
        }
        None => {
            send_inline(to, template_type, vars, app_name, app_url).await
        }
    }
}

fn get_default_subject(template_type: &str, app_name: &str) -> String {
    match template_type {
        "welcome" => format!("Welcome to {}!", app_name),
        "purchase_confirmed" => "Payment Received — Thank You!".to_string(),
        "password_reset" => "Password Reset Request".to_string(),
        _ => format!("{} Notification", app_name),
    }
}

async fn send_inline(to: &str, template_type: &str, vars: &serde_json::Value, app_name: &str, app_url: &str) -> Result<(), String> {
    let name = vars.get("name").and_then(|v| v.as_str()).unwrap_or("there");
    let email = vars.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = vars.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let token = vars.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let plan_name_val = vars.get("plan_name").and_then(|v| v.as_str()).unwrap_or("a plan");

    match template_type {
        "welcome" => {
            let body = format!(
                "Welcome to {}, {}!\n\nYour account has been created successfully.\n\nHere are your login credentials:\n\nEmail: {}\nPassword: {}\n\nLogin at: {}/login\n\nYou can now:\n- Create loyalty programs\n- Manage customer rewards\n- Track engagement metrics\n\nFor help, contact support@incentiveswift.com\n\nBest regards,\nThe {} Team",
                app_name, name, email, password, app_url, app_name
            );
            send_email_request(to, &format!("Welcome to {}!", app_name), &body, "").await
        }
        "purchase_confirmed" => {
            let body = format!(
                "Hi {},\n\nThank you for your purchase! Your payment for the {} plan has been received successfully.\n\nYou can access your dashboard at: {}/dashboard\n\nIf you have any questions, please contact support@incentiveswift.com\n\nBest regards,\nThe {} Team",
                name, plan_name_val, app_url, app_name
            );
            send_email_request(to, "Payment Received - Thank You!", &body, "").await
        }
        "password_reset" => {
            let body = format!(
                "Your password reset code is: {}\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- SwiftSoftware",
                token
            );
            send_email_request(to, "Password Reset Request", &body, "").await
        }
        _ => {
            let body = format!("{} Notification:\n\n{}", app_name, vars.to_string());
            send_email_request(to, &format!("{} Notification", app_name), &body, "").await
        }
    }
}

/// Keep original functions for backward compatibility
pub async fn send_welcome_email(to: &str, name: &str, password: &str) -> Result<(), String> {
    let body = format!(
        "Welcome to IncentiveSwift, {0}!\n\nYour account has been created successfully.\n\nHere are your login credentials:\n\nEmail: {1}\nPassword: {2}\n\nLogin at: https://app.incentiveswift.com/login\n\nYou can now:\n- Create loyalty programs\n- Manage customer rewards\n- Track engagement metrics\n\nFor help, contact support@incentiveswift.com\n\nBest regards,\nThe IncentiveSwift Team",
        name, to, password,
    );
    send_email_request(to, "Welcome to IncentiveSwift!", &body, "").await
}

pub async fn send_purchase_confirmed_email(to: &str, name: &str, plan_name: &str) -> Result<(), String> {
    let body = format!(
        "Hi {0},\n\nThank you for your purchase! Your payment for the {1} plan has been received successfully.\n\nYou can access your dashboard at: https://app.incentiveswift.com/dashboard\n\nIf you have any questions, please contact support@incentiveswift.com\n\nBest regards,\nThe IncentiveSwift Team",
        name, plan_name,
    );
    send_email_request(to, "Payment Received - Thank You!", &body, "").await
}

pub async fn send_reset_email(to: &str, token: &str) -> Result<(), String> {
    let body = format!(
        "Your password reset code is: {}\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- SwiftSoftware",
        token
    );
    send_email_request(to, "Password Reset Request", &body, "").await
}

/// Core email sender — form-based HTTP POST (Mailgun-compatible)
async fn send_email_request(to: &str, subject: &str, text_body: &str, html_body: &str) -> Result<(), String> {
    let api_url = env::var("EMAIL_API_URL")
        .map_err(|_| "EMAIL_API_URL not set".to_string())?;
    let api_key = env::var("EMAIL_API_KEY")
        .map_err(|_| "EMAIL_API_KEY not set".to_string())?;
    let from = env::var("EMAIL_FROM")
        .unwrap_or_else(|_| "swiftsoftware143@yahoo.com".to_string());

    let mut params = std::collections::HashMap::new();
    params.insert("from", from.as_str());
    params.insert("to", to);
    params.insert("subject", subject);
    params.insert("text", text_body);

    let client = reqwest::Client::new();
    let resp = client
        .post(&api_url)
        .basic_auth("api", Some(&api_key))
        .form(&params)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Failed to send email request: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Email API returned {}: {}", status, text));
    }

    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct EmailTemplateRow {
    id: uuid::Uuid,
    name: String,
    subject: Option<String>,
    body: Option<String>,
    html_body: Option<String>,
    is_html: Option<bool>,
    is_default: Option<bool>,
}
