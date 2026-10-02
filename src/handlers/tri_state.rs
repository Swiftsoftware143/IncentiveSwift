//! Tri-state request fields — `absent` / JSON `null` / value (kanban t_2371942d, t_6c8d8e40).
//!
//! `Option<Option<T>>` on its own does NOT distinguish three instructions. serde's `Option` impl
//! answers the OUTER `None` for JSON `null`, so `{"field": null}` is indistinguishable from a key
//! that was never sent — and a writer of the form `body.field.unwrap_or(existing.field)` (or a SQL
//! `COALESCE($n, field)`) legitimately reads both as "keep". There is then no spelling of the
//! request that CLEARS the column.
//!
//! The fleet census (kanban t_55c03596) measured five such request fields live in this app:
//! `campaign_secret_codes.expires_at`, `iqs_funnels.source_tag`, `iqs_questions.crm_field`,
//! `iqs_questions.crm_field_type`, `campaigns.loyalty_program_id` — each answered 200 to
//! `PUT {field: null}` while the column stayed put.
//!
//! Use as `#[serde(default, deserialize_with = "double_option")]`. `deserialize_with` is only
//! invoked when the key IS present, and the inner `Option` answers `None` for `null`; the outer
//! `Some` is what records "the caller spoke". `default` covers the absent key.
//!
//! This is the verified shape from t_6c8d8e40, moved here unchanged so more than one handler can
//! reuse it. Do not change its semantics.

use serde::Deserialize;

/// absent -> `None`, JSON `null` -> `Some(None)`, value -> `Some(Some(v))`.
pub(crate) fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "double_option")]
        field: Option<Option<String>>,
    }

    #[test]
    fn absent_null_and_value_are_three_instructions() {
        let absent: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.field, None, "absent key must mean 'do not mention'");

        let cleared: Probe = serde_json::from_str(r#"{"field": null}"#).unwrap();
        assert_eq!(
            cleared.field,
            Some(None),
            "JSON null must mean 'clear the column'"
        );

        let set: Probe = serde_json::from_str(r#"{"field": "v"}"#).unwrap();
        assert_eq!(set.field, Some(Some("v".to_string())));
    }
}
