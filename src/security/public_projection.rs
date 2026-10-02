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
//!
//! ## The second credential vocabulary: `campaigns.delivery_config` (kanban t_10559717)
//!
//! The SAME anonymous arms also used to emit the whole `campaigns.delivery_config` jsonb. That
//! column carries two credential vocabularies: `integrations[].config.{api_key,url}` and the flat
//! legacy `_method` / `api_type` / `api_key` / `webhook_url` (both read by
//! `handlers::entries::dispatch_integrations`), beside the hub's `delivery.{on_win,on_lose}` block.
//! A tenant who configured a direct-API delivery leg published that `api_key` and `webhook_url` to
//! any caller with no token at all (measured 2026-10-02 on the deployed binary, three arms).
//! `public_delivery_config` is the projection for that column.

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

/// The public projection of one campaign's `delivery_config`.
///
/// Why the empty object, decided per route (kanban t_10559717):
///
/// * **ZERO served shells read this column from an anonymous payload.** Measured on the box
///   2026-10-02: the only two readers of `delivery_config` are `www-admin/index.html`'s delivery
///   panel and `www-app/iqs.html`, and both read it from the AUTHENTICATED `GET /api/v1/campaigns`
///   (then merge the whole column back through an authenticated PUT). No public shell reads it —
///   `www/` has no match at all and the served `play.html` has no `delivery` reference — so there is
///   no key here to keep for a public consumer, and keeping one would be keeping a key nobody reads.
/// * **Every live campaign already emits `{}` on these routes** (all three rows carry
///   `delivery_config = '{}'`), so the empty object IS the shape the shells see today.
/// * The column's leaf set is open-ended (its writers can add keys), each known vocabulary carries a
///   credential, and the payload carries the non-secret `delivery_method` column separately — so the
///   projection is the whitelist with nothing on it. A key added later can never leak.
///
/// A non-object value (a string, array, number) projects to `{}` too: whatever is in that column,
/// a JSON string could itself be a secret. `null` passes through unchanged — there is nothing in it
/// to leak (the live column is `NOT NULL`, so a row read cannot produce it).
///
/// This is for the ANONYMOUS arms ONLY. The authenticated list/detail arms must keep the raw column:
/// `www-admin`'s delivery panel and `www-app/iqs.html` merge it and would lose (or write back an
/// empty) `integrations[]` / `coreswift.list_id` / `iqs` if it were projected there.
pub fn public_delivery_config(delivery_config: &Value) -> Value {
    if delivery_config.is_null() {
        return Value::Null;
    }
    Value::Object(Map::new())
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

    #[test]
    fn delivery_config_projection_drops_both_credential_vocabularies() {
        use super::public_delivery_config;
        let dc = json!({
            "delivery": {
                "on_win": {"redirect": {"url": "https://probe.invalid/win", "text": "You won"},
                           "email": {"subject": "s", "body_text": "b"},
                           "webhooks": ["target-1"], "autoresponder_fire": false},
                "on_lose": {"redirect": {"url": "https://probe.invalid/lose"}}
            },
            "integrations": [{"type": "webhook",
                              "config": {"url": "https://probe.invalid/hook",
                                         "api_key": "dc_int_key", "server_prefix": "us9"}}],
            "_method": "direct_api",
            "api_type": "hubspot",
            "api_key": "dc_flat_key",
            "webhook_url": "https://probe.invalid/flat",
            "coreswift": {"list_id": "list-1"},
            "iqs": {"note": "n"}
        });

        let projected = public_delivery_config(&dc);
        assert_eq!(projected, json!({}));
        let text = projected.to_string();
        for needle in [
            "dc_int_key",
            "dc_flat_key",
            "probe.invalid",
            "target-1",
            "list-1",
            "us9",
            "hubspot",
        ] {
            assert!(
                !text.contains(needle),
                "{needle} survived the public delivery_config projection"
            );
        }
    }

    #[test]
    fn delivery_config_projection_is_total_and_keeps_the_empty_shape() {
        use super::public_delivery_config;
        // The shape every live campaign already emits is byte-identical.
        assert_eq!(public_delivery_config(&json!({})), json!({}));
        // Nothing to leak in a null.
        assert_eq!(public_delivery_config(&json!(null)), json!(null));
        // A string / array / number in that column could itself be a secret — never passed through.
        assert_eq!(public_delivery_config(&json!("dc_flat_key")), json!({}));
        assert_eq!(public_delivery_config(&json!(["dc_flat_key"])), json!({}));
        assert_eq!(public_delivery_config(&json!(12345)), json!({}));
    }
}
