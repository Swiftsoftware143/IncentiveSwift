use std::env;
use serde_json::json;

/// Send a password reset email via HTTP API (Mailgun, SendGrid, SMTP.com, etc.)
/// Uses environment variables for configuration so you can swap providers.
///
/// Required env vars:
///   EMAIL_API_URL - The HTTP endpoint (default: uses Mailgun format)
///   EMAIL_API_KEY - API key for the provider
///   EMAIL_FROM    - From address (default: noreply@swiftsoftware.com)
///
/// Mailgun example:
///   EMAIL_API_URL=https://api.mailgun.net/v3/mg.swiftsoftware.com/messages
///   EMAIL_API_KEY=key-xxxxxxxxx
///
/// SMTP.com example:
///   EMAIL_API_URL=https://api.smtp.com/v4/messages
///   EMAIL_API_KEY=your-smtpcom-key

pub async fn send_reset_email(to: &str, token: &str) -> Result<(), String> {
    let api_url = env::var("EMAIL_API_URL")
        .map_err(|_| "EMAIL_API_URL not set".to_string())?;
    let api_key = env::var("EMAIL_API_KEY")
        .map_err(|_| "EMAIL_API_KEY not set".to_string())?;
    let from = env::var("EMAIL_FROM")
        .unwrap_or_else(|_| "noreply@swiftsoftware.com".to_string());

    let body = format!(
        "Your password reset code is: {}\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- SwiftSoftware",
        token
    );

    let payload = json!({
        "from": from,
        "to": to,
        "subject": "Password Reset Request",
        "text": body
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&api_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
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
