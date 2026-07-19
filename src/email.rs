use std::env;
use serde_json::json;

/// Send a password reset email via HTTP API (Mailgun, SendGrid, SMTP.com, etc.)
/// Uses environment variables for configuration so you can swap providers.
///
/// Required env vars:
///   EMAIL_API_URL - The HTTP endpoint (default: uses Mailgun format)
///   EMAIL_API_KEY - API key for the provider
///   EMAIL_FROM    - From address (default: swiftsoftware143@yahoo.com)
///
/// Mailgun example:
///   EMAIL_API_URL=https://api.mailgun.net/v3/mg.funnelswift.net/messages
///   EMAIL_API_KEY=key-xxxxxxxxx
///
/// SMTP.com example:
///   EMAIL_API_URL=https://api.smtp.com/v4/messages
///   EMAIL_API_KEY=your-smtpcom-key
/// Send a welcome email to a new user with their temporary password.
pub async fn send_welcome_email(to: &str, name: &str, password: &str) -> Result<(), String> {
    let api_url = env::var("EMAIL_API_URL")
        .map_err(|_| "EMAIL_API_URL not set".to_string())?;
    let api_key = env::var("EMAIL_API_KEY")
        .map_err(|_| "EMAIL_API_KEY not set".to_string())?;
    let from = env::var("EMAIL_FROM")
        .unwrap_or_else(|_| "swiftsoftware143@yahoo.com".to_string());

    let body = format!(
        "Welcome to IncentiveSwift, {0}!\n\nYour account has been created successfully.\n\nHere are your login credentials:\n\nEmail: {1}\nPassword: {2}\n\nLogin at: https://app.incentiveswift.com/login\n\nYou can now:\n- Create loyalty programs\n- Manage customer rewards\n- Track engagement metrics\n\nFor help, contact support@incentiveswift.com\n\nBest regards,\nThe IncentiveSwift Team",
        name,
        to,
        password,
    );

    let mut params = std::collections::HashMap::new();
    params.insert("from", from.as_str());
    params.insert("to", to);
    params.insert("subject", "Welcome to IncentiveSwift!");
    params.insert("text", &body);

    let client = reqwest::Client::new();
    let resp = client
        .post(&api_url)
        .basic_auth("api", Some(&api_key))
        .form(&params)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Failed to send welcome email: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Email API returned {}: {}", status, text));
    }

    Ok(())
}

/// Send a purchase confirmed email after successful payment.
pub async fn send_purchase_confirmed_email(to: &str, name: &str, plan_name: &str) -> Result<(), String> {
    let api_url = env::var("EMAIL_API_URL")
        .map_err(|_| "EMAIL_API_URL not set".to_string())?;
    let api_key = env::var("EMAIL_API_KEY")
        .map_err(|_| "EMAIL_API_KEY not set".to_string())?;
    let from = env::var("EMAIL_FROM")
        .unwrap_or_else(|_| "swiftsoftware143@yahoo.com".to_string());

    let body = format!(
        "Hi {0},\n\nThank you for your purchase! Your payment for the {1} plan has been received successfully.\n\nYou can access your dashboard at: https://app.incentiveswift.com/dashboard\n\nIf you have any questions, please contact support@incentiveswift.com\n\nBest regards,\nThe IncentiveSwift Team",
        name,
        plan_name,
    );

    let mut params = std::collections::HashMap::new();
    params.insert("from", from.as_str());
    params.insert("to", to);
    params.insert("subject", "Payment Received - Thank You!");
    params.insert("text", &body);

    let client = reqwest::Client::new();
    let resp = client
        .post(&api_url)
        .basic_auth("api", Some(&api_key))
        .form(&params)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Failed to send purchase confirmed email: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Email API returned {}: {}", status, text));
    }

    Ok(())
}

pub async fn send_reset_email(to: &str, token: &str) -> Result<(), String> {
    let api_url = env::var("EMAIL_API_URL")
        .map_err(|_| "EMAIL_API_URL not set".to_string())?;
    let api_key = env::var("EMAIL_API_KEY")
        .map_err(|_| "EMAIL_API_KEY not set".to_string())?;
    let from = env::var("EMAIL_FROM")
        .unwrap_or_else(|_| "swiftsoftware143@yahoo.com".to_string());

    let body = format!(
        "Your password reset code is: {}\n\nThis code expires in 1 hour.\n\nIf you did not request this password reset, please ignore this email.\n\n- SwiftSoftware",
        token
    );

    let mut params = std::collections::HashMap::new();
    params.insert("from", from.as_str());
    params.insert("to", to);
    params.insert("subject", "Password Reset Request");
    params.insert("text", &body);

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
