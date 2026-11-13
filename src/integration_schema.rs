//! `GET /api/v1/integration/schema` — the SOURCE half of the CoreSwift visual field mapper.
//!
//! **FLEET-CANONICAL COPY** — this file is byte-identical in every SwiftSoftware app. To add the
//! endpoint to an app you change NOTHING here: you add (1) a `pub const SCHEMA` config at crate
//! root, (2) one `.route("/api/v1/integration/schema", get(crate::integration_schema::integration_schema))`
//! line, and (3) the app's existing admin auth guard already applies. Verify parity across apps by
//! comparing the sha256 of this FILE (see the header note in each app's copy). Do not fork this
//! file; if one app needs different behaviour, change the config, not this module.
//!
//! Contract: /opt/swift/docs/coreswift-data-mapping-spec-2026-11-13.md §2. CoreSwift's mapper calls
//! this to list an app's REAL fields with MASKED sample values instead of a hardcoded list.
//! Response shape:
//!
//! ```json
//! {"app":"funnelswift","version":1,"entities":[{"name":"leads","label":"Leads",
//!   "sample_count":N,"fields":[{"path":"first_name","label":"First name","type":"string",
//!   "required":false,"sample":"John"}, ...]}]}
//! ```
//!
//! Rules this module honours:
//! * Field discovery is driven by Postgres introspection (`information_schema.columns`), never by a
//!   hardcoded column list: a column added to the table appears automatically with a fresh build.
//! * Columns named for credentials / secrets / PII are excluded (see [`is_excluded`]); the app
//!   config carries only `{entity key, label, table}`.
//! * `required` mirrors the column's NOT NULL constraint; `type` is one of the mapper's widget
//!   vocabulary — string/email/phone/date/number/array/object/bool — derived from the Postgres type
//!   and, for email/phone, the column name.
//! * Samples are MASKED by type (emails -> `j***@e***.com`, phones keep the last three digits,
//!   string leaves are scanned for embedded addresses/numbers). A `sample` is `null` when there is
//!   no data — never fabricated.
//! * Nested jsonb objects are flattened to dot paths (`custom_fields.<key>`) so the UI can render a
//!   collapsible tree; jsonb arrays (`tags`) stay whole.
//! * ADMIN ONLY: a non-admin caller is refused 403 in the handler; a missing credential is refused
//!   401 by the app's credential boundary before this handler runs.
//!
//! The `table` name in the config is a compile-time literal from the app's own source (never user
//! input) and is validated against an identifier shape before it is interpolated (see
//! [`validate_table`] and the two `count_sql` / `sample_sql` builders, which are the ONLY runtime
//! SQL assembly here and are covered by an earned gate-5d waiver per app).

use axum::{extract::State, Json};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The version of THIS FILE. Bump it when the file changes; every app must carry the same value.
/// The authoritative byte-identity check is the sha256 of the file, not this string.
pub const SCHEMA_MODULE_VERSION: &str = "1.0.0";

/// One entity the mapper may read. `table` is a literal from the app's own source.
pub struct EntityConfig {
    pub key: &'static str,
    pub label: &'static str,
    pub table: &'static str,
}

/// The app's describe config — the ONE app-specific datum this module reads (defined at crate root
/// as `pub const SCHEMA: &integration_schema::AppSchema = …`).
pub struct AppSchema {
    pub app: &'static str,
    pub version: i64,
    pub entities: &'static [EntityConfig],
}

/// `GET /api/v1/integration/schema` — the handler the router registers.
pub async fn integration_schema(
    auth: AuthUser,
    State(state): State<AppState>,
) -> AppResult<Json<Value>> {
    if !auth.is_admin {
        return Err(AppError::Forbidden("Admin access required".into()));
    }
    Ok(Json(describe(&state.pool, crate::SCHEMA).await?))
}

/// Build the full response for the app's config. Pure-ish: reads the DB, returns JSON.
pub async fn describe(pool: &PgPool, schema: &AppSchema) -> AppResult<Value> {
    let mut entities = Vec::with_capacity(schema.entities.len());
    for entity in schema.entities {
        entities.push(describe_entity(pool, entity).await?);
    }
    Ok(json!({
        "app": schema.app,
        "version": schema.version,
        "entities": entities,
    }))
}

struct Column {
    name: String,
    data_type: String,
    udt_name: String,
    nullable: bool,
}

async fn describe_entity(pool: &PgPool, entity: &EntityConfig) -> AppResult<Value> {
    validate_table(entity.table)?;

    // ── introspection: the real columns, in ordinal order (no hardcoded list) ────────────
    let columns: Vec<Column> = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT column_name, data_type, udt_name, is_nullable \
         FROM information_schema.columns \
         WHERE table_name = $1 AND table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY ordinal_position",
    )
    .bind(entity.table)
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)?
    .into_iter()
    .map(|(name, data_type, udt_name, is_nullable)| Column {
        name,
        data_type,
        udt_name,
        nullable: is_nullable == "NO",
    })
    .filter(|c| !is_excluded(&c.name))
    .collect();

    let sample_count: i64 = sqlx::query_scalar(&count_sql(entity.table))
        .fetch_one(pool)
        .await
        .map_err(AppError::Database)?;

    // One live row is enough: the mapper needs the SHAPE of each value, not the whole table.
    // With no rows this is `None` and every sample below is `null` (never fabricated).
    let sample: Option<Value> = sqlx::query_scalar(&sample_sql(entity.table))
        .fetch_optional(pool)
        .await
        .map_err(AppError::Database)?;

    let mut fields: Vec<Value> = Vec::new();
    for col in &columns {
        let ty = widget_type(&col.name, &col.data_type, &col.udt_name, sample.as_ref());
        let sample_val = sample
            .as_ref()
            .and_then(|row| row.get(&col.name))
            .map(|v| mask_value(ty, v));
        fields.push(json!({
            "path": col.name,
            "label": humanize(&col.name),
            "type": ty,
            "required": col.nullable,
            "sample": sample_val.unwrap_or(Value::Null),
        }));
    }

    // ── flatten nested jsonb OBJECTS to dot paths; jsonb arrays stay whole ────────────────
    for col in &columns {
        if !is_json(&col.data_type) {
            continue;
        }
        if let Some(obj) = sample
            .as_ref()
            .and_then(|row| row.get(&col.name))
            .and_then(|v| v.as_object())
        {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort_unstable();
            for key in keys {
                let (ty, sample_val) = infer_scalar(&obj[key]);
                fields.push(json!({
                    "path": format!("{}.{}", col.name, key),
                    "label": humanize(key),
                    "type": ty,
                    "required": false,
                    "sample": sample_val,
                }));
            }
        }
    }

    Ok(json!({
        "name": entity.key,
        "label": entity.label,
        "sample_count": sample_count,
        "fields": fields,
    }))
}

/// The only run-time SQL assembly in this module. `table` is a compile-time literal from the
/// app's config, validated by [`validate_table`]; it is never user input. Covered by an earned
/// gate-5d waiver per app (a parameterized query cannot name a `FROM` identifier).
fn count_sql(table: &str) -> String {
    format!("SELECT count(*) FROM {table}")
}

/// See [`count_sql`].
fn sample_sql(table: &str) -> String {
    format!("SELECT to_jsonb(t) FROM {table} t LIMIT 1")
}

/// Defence-in-depth: a `table` name that is not a plain (optionally schema-qualified) identifier
/// is a config bug, and it is refused before any SQL is built.
fn validate_table(table: &str) -> AppResult<()> {
    let ok = table.split('.').all(|part| {
        let mut chars = part.chars();
        match chars.next() {
            Some(first) => {
                (first.is_ascii_alphabetic() || first == '_')
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            None => false,
        }
    });
    if !ok {
        return Err(AppError::Internal(format!(
            "invalid table identifier in schema config: {table}"
        )));
    }
    Ok(())
}

/// Column names that never appear in a describe: credentials, secrets and PII identifiers.
/// Matched case-insensitively with underscores ignored for the substring tests, plus a
/// `_sealed` / `_hash` suffix arm.
fn is_excluded(column: &str) -> bool {
    let c = column.to_ascii_lowercase();
    let compact = c.replace('_', "");
    [
        "password", "secret", "token", "keyhash", "otp", "session", "apikey",
    ]
    .iter()
    .any(|bad| compact.contains(bad))
        || c.ends_with("_sealed")
        || c.ends_with("_hash")
}

fn is_json(data_type: &str) -> bool {
    let dt = data_type.to_ascii_lowercase();
    dt == "jsonb" || dt == "json"
}

fn is_email_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("email") || n.contains("mail")
}

fn is_phone_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("phone") || n.contains("mobile") || n.contains("cell")
}

/// Map a Postgres column to the mapper's widget type. `sample` (the live row's value, if any)
/// distinguishes a jsonb OBJECT from a jsonb ARRAY when the column type alone cannot.
fn widget_type(
    name: &str,
    data_type: &str,
    udt_name: &str,
    sample: Option<&Value>,
) -> &'static str {
    let dt = data_type.to_ascii_lowercase();
    let udt = udt_name.to_ascii_lowercase();

    if is_json(&dt) {
        return match sample {
            Some(Value::Array(_)) => "array",
            _ => "object",
        };
    }
    if dt == "array" || udt.starts_with('_') {
        return "array";
    }
    if is_email_like(name) {
        return "email";
    }
    if is_phone_like(name) {
        return "phone";
    }
    match dt.as_str() {
        "integer" | "bigint" | "smallint" | "numeric" | "decimal" | "real" | "double precision"
        | "money" => "number",
        "boolean" | "bool" => "bool",
        "timestamp with time zone"
        | "timestamp without time zone"
        | "timestamp"
        | "date"
        | "time with time zone"
        | "time without time zone"
        | "time" => "date",
        _ => "string",
    }
}

/// Mask a sample value by its widget type. email/phone mask the whole value; string leaves are
/// scanned for embedded addresses and numbers; array/object recurse; numbers/booleans/dates pass
/// through (not PII).
fn mask_value(ty: &str, v: &Value) -> Value {
    match ty {
        "email" => v
            .as_str()
            .map(|s| json!(mask_email(s)))
            .unwrap_or(Value::Null),
        "phone" => v
            .as_str()
            .map(|s| json!(mask_phone(s)))
            .unwrap_or(Value::Null),
        "array" | "object" => mask_json(v),
        "string" => v
            .as_str()
            .map(|s| json!(mask_free_text(s)))
            .unwrap_or_else(|| v.clone()),
        _ => v.clone(),
    }
}

/// Infer the widget type for a flattened custom-field leaf and mask its string sample.
fn infer_scalar(v: &Value) -> (&'static str, Value) {
    match v {
        Value::String(s) => {
            if looks_like_email(s) {
                ("email", json!(mask_email(s)))
            } else {
                ("string", json!(mask_free_text(s)))
            }
        }
        Value::Number(_) => ("number", v.clone()),
        Value::Bool(_) => ("bool", v.clone()),
        Value::Array(_) => ("array", mask_json(v)),
        Value::Object(_) => ("object", mask_json(v)),
        Value::Null => ("string", Value::Null),
    }
}

/// `j***@e***.com` — the first char of the local part, the first char of the domain, and the TLD.
/// The rest is shape, never the address.
fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return mask_free_text(email);
    };
    let first_local = local.chars().next().map(String::from).unwrap_or_default();
    let first_domain = domain.chars().next().map(String::from).unwrap_or_default();
    let tld = domain
        .rsplit_once('.')
        .map(|(_, t)| format!(".{t}"))
        .unwrap_or_default();
    format!("{first_local}***@{first_domain}***{tld}")
}

/// `+1***1234` — a leading `+` and the country-code digits survive, the middle is masked, the last
/// three digits are kept. Never returns more than a 3-digit run, so a full number cannot leak.
fn mask_phone(phone: &str) -> String {
    let trimmed = phone.trim();
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return String::new();
    }
    let last3: String = digits
        .chars()
        .rev()
        .take(3)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    // Digits beyond a trailing 10-digit local number are the country code; keep those plus a `+`.
    let cc_len = digits.len().saturating_sub(10);
    let cc: String = digits.chars().take(cc_len).collect();
    let plus = if trimmed.starts_with('+') { "+" } else { "" };
    format!("{plus}{cc}***{last3}")
}

fn looks_like_email(s: &str) -> bool {
    let trimmed = s.trim_matches(|c: char| c.is_ascii_punctuation());
    if trimmed.chars().filter(|c| *c == '@').count() != 1 {
        return false;
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// Redact email and phone-like tokens inside free text (notes, custom-field string leaves), so no
/// full address or number escapes in a value this module did not type-check as a field.
fn mask_free_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut first = true;
    for token in text.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        if looks_like_email(token) {
            out.push_str(&mask_email(token));
        } else {
            let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() >= 7 {
                out.push_str(&mask_phone(token));
            } else {
                out.push_str(token);
            }
        }
    }
    out
}

/// Mask every string leaf of an arbitrary jsonb value (tags arrays, custom-fields objects).
fn mask_json(v: &Value) -> Value {
    match v {
        Value::String(s) => json!(mask_free_text(s)),
        Value::Array(a) => Value::Array(a.iter().map(mask_json).collect()),
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, val) in o {
                m.insert(k.clone(), mask_json(val));
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

/// `utm_source` -> `Utm source` — a readable label for a column or a flattened custom-field key.
fn humanize(key: &str) -> String {
    let words: Vec<String> = key
        .split(['_', '-'])
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The masking contract (spec §2): emails -> `j***@e***.com`, phones keep the last three
    /// digits (`+1***1234`), and neither output can carry a full address or a 7+ digit run.
    #[test]
    fn emails_and_phones_are_masked_to_shape_never_pii() {
        assert_eq!(mask_email("john@example.com"), "j***@e***.com");
        assert_eq!(mask_email("jane.doe@acme-corp.io"), "j***@a***.io");
        // the mask keeps only SHAPE — the real local part and domain never survive
        let masked_email = mask_email("jane.doe@acme-corp.io");
        assert!(
            !masked_email.contains("jane.doe"),
            "local part leaked: {masked_email}"
        );
        assert!(
            !masked_email.contains("acme-corp"),
            "domain leaked: {masked_email}"
        );

        let masked = mask_phone("+1 (555) 123-4567");
        assert!(masked.starts_with("+1"));
        assert!(masked.ends_with("***567"));
        let digits: String = masked.chars().filter(|c| c.is_ascii_digit()).collect();
        assert!(digits.len() < 7, "masked phone leaked digits: {masked}");

        assert_eq!(mask_phone("5551234567"), "***567");
    }

    /// Free text (notes, custom-field string leaves) must redact embedded addresses and numbers.
    #[test]
    fn free_text_redacts_embedded_emails_and_phones() {
        let text = "call me at 5551234567 or mail john@example.com thanks";
        let masked = mask_free_text(text);
        assert!(!masked.contains("5551234567"), "phone leaked: {masked}");
        assert!(
            !masked.contains("john@example.com"),
            "email leaked: {masked}"
        );
        assert!(masked.contains("call") && masked.contains("thanks"));
    }

    /// Column-name heuristics and type mapping drive the widget vocabulary.
    #[test]
    fn type_mapping_and_column_heuristics_are_derived_not_hardcoded() {
        assert_eq!(
            widget_type("email", "character varying", "varchar", None),
            "email"
        );
        assert_eq!(
            widget_type("phone", "character varying", "varchar", None),
            "phone"
        );
        assert_eq!(widget_type("score", "integer", "int4", None), "number");
        assert_eq!(widget_type("is_active", "boolean", "bool", None), "bool");
        assert_eq!(
            widget_type(
                "created_at",
                "timestamp with time zone",
                "timestamptz",
                None
            ),
            "date"
        );
        assert_eq!(
            widget_type("name", "character varying", "varchar", None),
            "string"
        );
        assert_eq!(
            widget_type("tags", "jsonb", "jsonb", Some(&json!(["a"]))),
            "array"
        );
        assert_eq!(
            widget_type("custom_fields", "jsonb", "jsonb", Some(&json!({"k": "v"}))),
            "object"
        );

        assert!(is_excluded("password_hash"));
        assert!(is_excluded("api_key_sealed"));
        assert!(is_excluded("session_token"));
        assert!(!is_excluded("first_name"));
        assert!(!is_excluded("email"));
    }

    /// Flattened custom-fields keys get a readable label and the right widget type.
    #[test]
    fn nested_jsonb_flattens_to_dot_paths_with_widget_types() {
        assert_eq!(humanize("utm_source"), "Utm Source");
        assert_eq!(humanize("lead-score"), "Lead Score");
        assert_eq!(infer_scalar(&json!("x")).0, "string");
        assert_eq!(infer_scalar(&json!(3)).0, "number");
        assert_eq!(infer_scalar(&json!(true)).0, "bool");
        assert_eq!(infer_scalar(&json!(["a"])).0, "array");
        assert_eq!(infer_scalar(&json!({"k": "v"})).0, "object");
    }
}
