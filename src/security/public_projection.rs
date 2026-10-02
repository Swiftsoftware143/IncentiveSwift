//! Public-surface projection redaction (kanban t_eeed1bba).
//!
//! `campaigns.config` is a tenant-owned jsonb blob. One of its keys, `marketing_boost`, holds the
//! tenant's Marketing Boost credential: legacy webhook mode stores `webhook_url` (a capability URL
//! — possession of it can drive the tenant's webhook) plus `auth_header_name`/`auth_header_value`,
//! and direct-API mode stores `api_key`. The anonymous surface routes used to answer with that blob
//! verbatim, so any caller with NO token could read a tenant's marketing credential.
//!
//! Measured live 2026-10-02 (binary 4536cd668290b2fe, harness
//! `/opt/swift/audits/t_10ee665b/public-projection-census.py`): a probe tenant's
//! `marketing_boost.auth_header_value` appeared in the anonymous body of
//! `GET /api/v1/campaigns/subdomain/:t_slug`, `GET /api/v1/play/:id` (uuid and slug),
//! `GET /api/v1/embed/campaign/:slug` and `GET /api/v1/embed/campaign/all` (which lists EVERY
//! account's active campaigns). The same blob is emitted by `GET /api/v1/widget/:hash/config` and
//! `GET /api/v1/embed/:id` (code-read, same shape).
//!
//! ZERO served shells read `config.marketing_boost` (measured: the only matches for
//! `marketing_boost` under the served roots are the RETIRED `www-app/archive` SPA, which is not
//! routed, and the admin console's Marketing Boost producer panel reads the authenticated,
//! owner-scoped `GET /api/v1/campaigns/:slug/marketing-boost`, which keeps returning the full
//! block). So the public projection keeps only the two non-secret facts a viewer could branch on
//! (`enabled`, `label`) and drops every credential / endpoint / delivery key.
//!
//! Everything else in `config` (`sections`, `rules`, `prize_pool`, `formula`, `cta_text`,
//! `entry_webhook_url`, … — the keys the served play / IQS / long-form shells actually read) is
//! emitted byte-for-byte unchanged, so the response shape the shells see does not move.

use serde_json::{Map, Value};

/// The public projection of one campaign's `config`.
///
/// A config without a `marketing_boost` object (or a non-object config) is returned UNCHANGED —
/// byte-for-byte, including key order — so only campaigns that actually configured Marketing Boost
/// can see any difference at all.
pub fn public_config(config: &Value) -> Value {
    let Some(map) = config.as_object() else {
        return config.clone();
    };
    let Some(boost) = map.get("marketing_boost").and_then(Value::as_object) else {
        return config.clone();
    };

    // Whitelist, not blacklist: a field added to the boost block later can never leak by default.
    let mut safe = Map::new();
    if let Some(enabled) = boost.get("enabled") {
        safe.insert("enabled".to_string(), enabled.clone());
    }
    if let Some(label) = boost.get("label") {
        safe.insert("label".to_string(), label.clone());
    }

    let mut out = map.clone();
    out.insert("marketing_boost".to_string(), Value::Object(safe));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::public_config;
    use serde_json::json;

    #[test]
    fn drops_the_credential_block_and_keeps_the_rest() {
        let config = json!({
            "marketing_boost": {
                "enabled": true,
                "label": "Marketing Boost",
                "events": ["on_win"],
                "webhook_url": "https://probe.invalid/pub",
                "auth_header_name": "X-Probe",
                "auth_header_value": "probe-pub-secret",
                "api_key": "mb_secret_key",
                "sender": "3822-4706",
                "business": "6111"
            },
            "sections": [{"title": "Step 1"}],
            "passing_score": 70
        });

        let projected = public_config(&config);
        let boost = &projected["marketing_boost"];
        assert_eq!(boost["enabled"], json!(true));
        assert_eq!(boost["label"], json!("Marketing Boost"));
        for field in [
            "webhook_url",
            "auth_header_name",
            "auth_header_value",
            "api_key",
            "events",
            "sender",
            "business",
            "incentive_type",
            "amount",
            "destination",
            "trigger_events",
        ] {
            assert!(
                boost.get(field).is_none(),
                "{field} survived the public projection"
            );
        }
        // Non-boost keys are untouched.
        assert_eq!(projected["sections"], config["sections"]);
        assert_eq!(projected["passing_score"], json!(70));
    }

    #[test]
    fn a_config_without_marketing_boost_is_identical() {
        let config = json!({"sections": [], "formula": "a+b", "cta_text": "Enter now"});
        assert_eq!(public_config(&config), config);
    }

    #[test]
    fn a_non_object_config_is_identical() {
        assert_eq!(public_config(&json!(null)), json!(null));
        assert_eq!(public_config(&json!([1, 2])), json!([1, 2]));
    }

    #[test]
    fn a_disabled_boost_block_is_never_resurrected() {
        // `set_marketing_boost` REMOVES the key when disabled, so a present block is always an
        // object; if one ever arrives without `enabled`, it must still lose the credential.
        let config = json!({"marketing_boost": {"auth_header_value": "still-secret"}});
        let projected = public_config(&config);
        assert_eq!(projected["marketing_boost"], json!({}));
    }
}
