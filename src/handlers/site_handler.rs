use axum::extract::{Json, State};
use serde_json::json;
use std::fs;
use uuid::Uuid;

use sqlx::Row;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

const SITE_KEY: &str = "incentiveswift_site";

/// GET /api/v1/admin/site — get site settings (SEO, tracking, homepage, legal)
pub async fn get_site(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
) -> Result<Json<serde_json::Value>, AppError> {
    let defaults = default_site_settings();

    let row = sqlx::query("SELECT value FROM admin_settings WHERE key = $1")
        .bind(SITE_KEY)
        .fetch_optional(&state.db)
        .await?;

    let settings = match row {
        Some(r) => {
            let val: serde_json::Value = r.try_get("value")?;
            merge_json(defaults, val)
        }
        None => defaults,
    };

    Ok(Json(settings))
}

/// PUT /api/v1/admin/site — update site settings & regenerate marketing HTML
pub async fn update_site(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Merge with existing
    let existing_row = sqlx::query("SELECT value FROM admin_settings WHERE key = $1")
        .bind(SITE_KEY)
        .fetch_optional(&state.db)
        .await?;

    let merged = match existing_row {
        Some(r) => {
            let existing_val: serde_json::Value = r.try_get("value")?;
            merge_json(existing_val, req)
        }
        None => req,
    };

    let admin_id = Uuid::parse_str(&_auth.account_id).unwrap_or(Uuid::nil());

    sqlx::query(
        r#"INSERT INTO admin_settings (key, value, description, updated_at, updated_by)
           VALUES ($1, $2::jsonb, 'IncentiveSwift site settings (SEO, tracking, homepage, legal)', NOW(), $3)
           ON CONFLICT (key) DO UPDATE SET value = $2::jsonb, updated_at = NOW(), updated_by = $3"#
    )
    .bind(SITE_KEY)
    .bind(merged.to_string())
    .bind(admin_id)
    .execute(&state.db)
    .await?;

    // Regenerate marketing HTML
    regenerate_html(&merged)?;

    Ok(Json(json!({"message": "Site settings updated"})))
}

fn regenerate_html(settings: &serde_json::Value) -> Result<(), AppError> {
    let html_path = "/opt/swift/nginx/www/incentiveswift/index.html";
    let html = fs::read_to_string(html_path)
        .map_err(|e| AppError::Internal(format!("Failed to read {}: {}", html_path, e)))?;

    let html = inject_site_settings(&html, settings);

    fs::write(html_path, &html)
        .map_err(|e| AppError::Internal(format!("Failed to write {}: {}", html_path, e)))?;

    // Also regenerate legal pages
    if let (Some(tos), Some(privacy), Some(refunds_val)) = (
        settings.get("legal_tos").and_then(|v| v.as_str()),
        settings.get("legal_privacy").and_then(|v| v.as_str()),
        settings.get("legal_refunds").and_then(|v| v.as_str()),
    ) {
        regenerate_legal("terms", "Terms of Service", tos)?;
        regenerate_legal("privacy", "Privacy Policy", privacy)?;
        regenerate_legal("refunds", "Refund & Cancellation Policy", refunds_val)?;
    }

    Ok(())
}

fn regenerate_legal(slug: &str, title: &str, text: &str) -> Result<(), AppError> {
    let path = format!("/opt/swift/nginx/www/incentiveswift/{}.html", slug);
    let page = format!(
        r#"<!DOCTYPE html><html lang="en">
<head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{} — IncentiveSwift</title>
<style>body{{font-family:system-ui,sans-serif;background:#0f0f0f;color:#e5e5e5;line-height:1.7;margin:0;padding:0}}
.container{{max-width:800px;margin:0 auto;padding:60px 24px}}
h1{{font-size:2rem;color:#8b5cf6}}a{{color:#8b5cf6}}</style>
</head><body><div class="container"><h1>{}</h1>{}</div></body></html>"#,
        title, title, text
    );
    fs::write(&path, &page)
        .map_err(|e| AppError::Internal(format!("Failed to write {}: {}", path, e)))?;
    Ok(())
}

fn inject_site_settings(html: &str, s: &serde_json::Value) -> String {
    let mut result = html.to_string();

    if let Some(title) = s.get("title").and_then(|v| v.as_str()) {
        replace_title(&mut result, title);
    }
    if let Some(desc) = s.get("description").and_then(|v| v.as_str()) {
        upsert_meta(&mut result, "description", desc);
    }
    if let Some(kw) = s.get("keywords").and_then(|v| v.as_str()) {
        upsert_meta(&mut result, "keywords", kw);
    }
    upsert_meta_prop(
        &mut result,
        "og:title",
        s.get("og_title").and_then(|v| v.as_str()),
    );
    upsert_meta_prop(
        &mut result,
        "og:description",
        s.get("og_description").and_then(|v| v.as_str()),
    );
    upsert_meta_prop(
        &mut result,
        "og:image",
        s.get("og_image_url").and_then(|v| v.as_str()),
    );

    if let Some(schema_json) = s.get("schema_json").and_then(|v| v.as_str()) {
        if !schema_json.is_empty() {
            upsert_schema(&mut result, schema_json);
        }
    }

    let ga_id = s.get("ga_id").and_then(|v| v.as_str()).unwrap_or("");
    let gtm_id = s.get("gtm_id").and_then(|v| v.as_str()).unwrap_or("");
    remove_ga_gtm(&mut result);

    if !ga_id.is_empty() {
        let ga_script = format!(
            r#"<script async src="https://www.googletagmanager.com/gtag/js?id={}"></script><script>window.dataLayer=window.dataLayer||[];function gtag(){{dataLayer.push(arguments);}}gtag('js',new Date());gtag('config','{}');</script>"#,
            ga_id, ga_id
        );
        inject_before_head_end(&mut result, &ga_script);
    }
    if !gtm_id.is_empty() {
        let gtm_head = format!(
            r#"<script>(function(w,d,s,l,i){{w[l]=w[l]||[];w[l].push({{'gtm.start':new Date().getTime(),event:'gtm.js'}});var f=d.getElementsByTagName(s)[0],j=d.createElement(s);j.async=true;j.src='https://www.googletagmanager.com/gtm.js?id='+i;f.parentNode.insertBefore(j,f);}})(window,document,'script','dataLayer','{}');</script>"#,
            gtm_id
        );
        inject_before_head_end(&mut result, &gtm_head);
    }
    if let Some(head_scripts) = s.get("head_scripts").and_then(|v| v.as_str()) {
        if !head_scripts.is_empty() {
            inject_before_head_end(&mut result, head_scripts);
        }
    }
    if let Some(body_scripts) = s.get("body_scripts").and_then(|v| v.as_str()) {
        if !body_scripts.is_empty() {
            inject_before_body_end(&mut result, body_scripts);
        }
    }

    result
}

fn replace_title(result: &mut String, new_title: &str) {
    let open = "<title>";
    let close = "</title>";
    if let Some(start) = result.find(open) {
        let after_open = start + open.len();
        if let Some(end) = result[after_open..].find(close) {
            result.replace_range(after_open..after_open + end, new_title);
        }
    } else {
        inject_before_head_end(result, &format!("<title>{}</title>", new_title));
    }
}

fn upsert_meta(result: &mut String, name: &str, content: &str) {
    let pattern = format!(r#"<meta name="{}""#, name);
    if let Some(pos) = result.find(&pattern) {
        let after = &result[pos..];
        if let Some(end) = after.find('>') {
            result.replace_range(
                pos..pos + end + 1,
                &format!(r#"<meta name="{}" content="{}">"#, name, content),
            );
        }
    } else {
        inject_before_head_end(
            result,
            &format!(r#"<meta name="{}" content="{}">"#, name, content),
        );
    }
}

fn upsert_meta_prop(result: &mut String, property: &str, content: Option<&str>) {
    if let Some(c) = content {
        let pattern = format!(r#"<meta property="{}""#, property);
        if let Some(pos) = result.find(&pattern) {
            let after = &result[pos..];
            if let Some(end) = after.find('>') {
                result.replace_range(
                    pos..pos + end + 1,
                    &format!(r#"<meta property="{}" content="{}">"#, property, c),
                );
            }
        } else {
            inject_before_head_end(
                result,
                &format!(r#"<meta property="{}" content="{}">"#, property, c),
            );
        }
    }
}

fn upsert_schema(result: &mut String, schema_json: &str) {
    let open = r#"<script type="application/ld+json">"#;
    let close = r#"</script>"#;
    if let Some(start) = result.find(open) {
        let after_open = start + open.len();
        if let Some(end) = result[after_open..].find(close) {
            result.replace_range(after_open..after_open + end, schema_json);
        }
    } else {
        inject_before_head_end(
            result,
            &format!(
                r#"<script type="application/ld+json">{}</script>"#,
                schema_json
            ),
        );
    }
}

fn remove_ga_gtm(result: &mut String) {
    let patterns = [
        (
            r#"<script async src="https://www.googletagmanager.com/gtag/js"#,
            "</script>",
        ),
        (r#"<script>window.dataLayer=window.dataLayer"#, "</script>"),
        (
            r#"<script>(function(w,d,s,l,i){w[l]=w[l]||[];w[l].push"#,
            "</script>",
        ),
        (
            r#"<noscript><iframe src="https://www.googletagmanager.com/ns.html"#,
            "</noscript>",
        ),
    ];
    for (start_pat, end_pat) in &patterns {
        loop {
            if let Some(pos) = result.find(start_pat) {
                if let Some(end) = result[pos..].find(end_pat) {
                    result.replace_range(pos..pos + end + end_pat.len(), "");
                    continue;
                }
            }
            break;
        }
    }
    while result.contains("\n\n\n") {
        *result = result.replace("\n\n\n", "\n\n");
    }
}

fn inject_before_head_end(result: &mut String, content: &str) {
    if let Some(pos) = result.rfind("</head>") {
        result.insert_str(pos, &format!("\n  {}", content));
    }
}

fn inject_before_body_end(result: &mut String, content: &str) {
    if let Some(pos) = result.rfind("</body>") {
        result.insert_str(pos, &format!("\n  {}", content));
    }
}

fn merge_json(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    match (a, b) {
        (serde_json::Value::Object(mut a_map), serde_json::Value::Object(b_map)) => {
            for (k, v) in b_map {
                a_map.insert(k, v);
            }
            serde_json::Value::Object(a_map)
        }
        (_a, b) => b,
    }
}

fn default_site_settings() -> serde_json::Value {
    json!({
        "title": "IncentiveSwift | Viral Campaigns & Loyalty Rewards Platform",
        "description": "Create viral incentive campaigns, loyalty programs, raffles, and sweepstakes with IncentiveSwift. Boost engagement and retention with gamified rewards.",
        "keywords": "incentive platform, viral campaigns, loyalty rewards, raffle software, sweepstakes, gamification",
        "og_title": "IncentiveSwift — Viral Campaigns & Loyalty Rewards",
        "og_description": "Create viral incentive campaigns, loyalty programs, raffles, and sweepstakes that drive engagement.",
        "og_image_url": "",
        "favicon_url": "",
        "canonical_url": "https://incentiveswift.com",
        "ga_id": "",
        "gtm_id": "",
        "head_scripts": "",
        "body_scripts": "",
        "schema_json": "{\"@context\":\"https://schema.org\",\"@type\":\"SoftwareApplication\",\"name\":\"IncentiveSwift\",\"operatingSystem\":\"All\",\"applicationCategory\":\"BusinessApplication\",\"offers\":{\"@type\":\"Offer\",\"price\":\"0.00\",\"priceCurrency\":\"USD\"},\"description\":\"Viral incentive campaigns, loyalty programs, raffles, and sweepstakes platform.\"}",
        "legal_tos": "",
        "legal_privacy": "",
        "legal_refunds": "",
        "homepage": {
            "logo_text": "IncentiveSwift",
            "headline": "Create Viral Campaigns That Drive Results",
            "subheadline": "Loyalty rewards, raffles, sweepstakes, and more — all in one platform.",
            "button_text": "Get Started Free",
            "secondary_button_text": "View Demo"
        }
    })
}
