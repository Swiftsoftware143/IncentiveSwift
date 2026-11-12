use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

// ── Data Retention Types ──

/// Response returned when querying a tenant's retention settings.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionResponse {
    pub tenant_id: Uuid,
    pub retention_days: i32,
    pub last_purged_at: Option<DateTime<Utc>>,
}

/// Request body for setting a tenant's email retention period.
/// Minimum: 30 days, Maximum: 3650 days (10 years).
#[derive(Debug, Deserialize)]
pub struct SetRetentionRequest {
    #[serde(default = "default_retention")]
    pub retention_days: i32,
}

fn default_retention() -> i32 {
    // The backstop, not the product rule (B206 revised): ten years, so an unset window never deletes
    // a customer's mail on a clock. An admin may still set a shorter window deliberately.
    3650
}

/// Request body for triggering a manual purge.
#[derive(Debug, Deserialize)]
pub struct PurgeTriggerRequest {
    /// If provided, limits the purge to a single tenant.
    /// If omitted (agency_admin only), purges all tenants.
    pub tenant_id: Option<Uuid>,
}

/// Summary of what the purge task accomplished.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeSummaryResponse {
    pub tenants_checked: i64,
    pub tenants_purged: i64,
    /// Outbound `outbound_messages` rows removed (the delivery log, not mail the customer reads).
    pub messages_deleted: i64,
    /// Stored inbox messages removed by the retention BACKSTOP (B206: ten years by default, so in
    /// normal operation this is 0 — nothing is deleted on a clock, and never to make room).
    pub inbox_messages_deleted: i64,
    pub events_deleted: i64,
    pub errors: Vec<String>,
}

impl Default for PurgeSummaryResponse {
    fn default() -> Self {
        Self::new()
    }
}

impl PurgeSummaryResponse {
    pub fn new() -> Self {
        Self {
            tenants_checked: 0,
            tenants_purged: 0,
            messages_deleted: 0,
            inbox_messages_deleted: 0,
            events_deleted: 0,
            errors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PrivateEmailDomain {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub domain: String,
    /// Credential columns: NEVER serialized (t_72f0bc83). `list_domains`/`update_domain` answer with
    /// `serde_json::to_value(&domain)`, so before this these fields shipped the tenant's stored
    /// ciphertext — and, on a legacy unsealed row, its PLAINTEXT — to the browser, while no client
    /// reads them (0 references in either served shell). The status of a credential is now reported
    /// by `credential_status`, which is computed by opening the value, never by echoing it.
    #[serde(skip_serializing)]
    pub mailgun_api_key: String,
    pub mailgun_region: String,
    pub catch_all_enabled: bool,
    pub verified: bool,
    pub label: Option<String>,
    pub api_key_id: Option<Uuid>,
    pub provider_type: String,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<i32>,
    pub smtp_username: Option<String>,
    #[serde(skip_serializing)]
    pub smtp_password_encrypted: Option<String>,
    pub smtp_tls: bool,
    pub inbound_mode: String,
    /// HOW THIS DOMAIN RECEIVES (kanban t_04db9228): `mailgun` | `sendgrid` | `none`.
    ///
    /// Deliberately separate from `provider_type`, which answers how it SENDS. A customer may send
    /// through Amazon SES (over SMTP) and receive through Mailgun — or send through Mailgun and
    /// receive through SendGrid — and neither choice may overwrite the other. `none` is a
    /// sending-only domain: it has no receiver, and a delivery addressed to it cannot validate.
    pub inbound_provider: String,
    /// The SendGrid Inbound Parse receiver's credential: the unguessable path segment of
    /// `/api/v1/webhooks/sendgrid/inbound/<token>`. NEVER serialized on the row — the setup surface
    /// answers it as a complete URL (`inbound_parse_url`) and nowhere else, so it cannot leak into a
    /// generic domain list.
    #[serde(skip_serializing)]
    pub inbound_token: Option<String>,

    #[serde(skip_serializing)]
    pub webhook_signing_key_encrypted: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PrivateEmailBox {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub domain_id: Uuid,
    pub user_id: Option<Uuid>,
    pub local_part: String,
    pub email_address: String,
    pub mailgun_mailbox_id: Option<String>,
    pub forwarding_enabled: bool,
    pub signature: Option<String>,
    /// This address's signature choice (migration 143, kanban t_b671a0f7): `inherit` uses the
    /// workspace default, `custom` uses the five columns below (blank fields fall back per field),
    /// `off` adds no signature at all.
    pub signature_mode: String,
    pub signature_full_name: Option<String>,
    pub signature_job_title: Option<String>,
    pub signature_company: Option<String>,
    pub signature_phone: Option<String>,
    pub signature_line: Option<String>,
    pub status: String,
    /// Whether mail to this address ALSO creates a ticket (migration 125). Every message reaches
    /// the inbox either way; this is the routing decision on top of it, and the user can tick or
    /// untick any mailbox (B204).
    pub creates_ticket: bool,
    /// The FREE-plan trial state of this mailbox (migration 136): `paid` — the account's plan sells
    /// sending; `trial` — inside the full send+receive window; `receive_only` — the window ended and
    /// the free plan no longer sends from this address (mail keeps arriving and nothing is deleted).
    /// The daily sweep maintains it for the console's benefit; the SEND decision itself is derived
    /// from the plan plus `trial_ends_at` (`trial::send_access`), so an upgrade restores sending on
    /// the very next request without waiting for any job.
    pub access_mode: String,
    pub trial_started_at: Option<DateTime<Utc>>,
    /// When the free trial's full function ends. NULL for a mailbox whose plan sells sending.
    pub trial_ends_at: Option<DateTime<Utc>>,
    /// When the sweep actually flipped this mailbox to receive-only (the audit stamp).
    pub trial_expired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Request/response types

#[derive(Debug, Deserialize)]
pub struct AddDomainRequest {
    pub domain: String,
    pub label: Option<String>,
    #[serde(default)]
    pub mailgun_api_key: Option<String>,
    pub api_key_id: Option<Uuid>,
    #[serde(default = "default_region")]
    pub mailgun_region: String,
    // Provider type selection
    #[serde(default = "default_provider_type")]
    pub provider_type: String,
    // SMTP provider config (when provider_type = "smtp")
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub smtp_port: Option<i32>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    #[serde(default = "default_true")]
    pub smtp_tls: bool,
    /// HOW THIS DOMAIN SENDS (kanban t_04db9228). `mailgun` (their own Mailgun account, over its
    /// API) or `smtp` (any SMTP service: Amazon SES, SendGrid, Postmark, Google Workspace, a
    /// host's own mail server). Absent falls back to `provider_type` so every existing caller —
    /// and the older panel that only ever posted `provider_type` — keeps working unchanged.
    ///
    /// Sending and receiving are asked separately on purpose: the customer is never told to move
    /// their outbound mail in order to receive with us.
    #[serde(default)]
    pub sending_provider: Option<String>,
    /// HOW THIS DOMAIN RECEIVES (kanban t_04db9228). `mailgun` or `sendgrid` — the only two
    /// receivers that exist in this binary — or `none` for a sending-only domain. Anything else is
    /// refused (400) rather than stored, because a stored value no receiver answers to is a
    /// dropdown entry that does nothing.
    ///
    /// Absent means "the same as the sending provider": `mailgun` for a Mailgun domain, `none` for
    /// an SMTP one — which is exactly what every domain added before today does.
    #[serde(default)]
    pub receiving_provider: Option<String>,

    /// The domain's Mailgun WEBHOOK signing key (kanban t_fbd607a7), the credential
    /// `POST /api/v1/webhooks/mailgun/inbound` authenticates a delivery with
    /// (`providers::mailgun::accept_inbound`). Mailgun presents no session and cannot carry an
    /// internal key, so this signature IS the receiver's only credential — and until now nothing
    /// in the app could ever write the column that holds it, which made this domain's inbound
    /// mail permanently refused (t_7f134e77 made the receiver fail closed on it).
    ///
    /// Optional: a sending-only domain needs no key. Sealed on write through the app-wide
    /// envelope, never stored plaintext, and never echoed back (the column is
    /// `#[serde(skip_serializing)]`; the read path reports presence + length only).
    #[serde(default)]
    pub webhook_signing_key: Option<String>,
}

fn default_region() -> String {
    "us".into()
}
fn default_provider_type() -> String {
    "mailgun".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ProvisionMailboxRequest {
    pub domain_id: Uuid,
    pub local_part: String,
    pub user_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct SendEmailRequest {
    pub from_address: String,
    pub to: String,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    /// Whether the signature block goes on THIS message. Absent means yes — the composer opens with
    /// the signature already in place, so the default has to match what the sender saw.
    #[serde(default)]
    pub include_signature: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDomainRequest {
    pub catch_all_enabled: Option<bool>,
    /// Re-enter a domain's own Mailgun credential (t_72f0bc83). A stored key this deployment cannot
    /// open made the domain permanently unusable: the send path fails closed with "…cannot be read
    /// — re-add the domain", and there was no way to re-add one without deleting it (and every
    /// mailbox on it). Sealed on write; empty/omitted leaves the stored value alone.
    #[serde(default)]
    pub mailgun_api_key: Option<String>,
    /// Re-enter a domain's SMTP password (`provider_type = "smtp"`), same contract as above.
    #[serde(default)]
    pub smtp_password: Option<String>,
    /// Switch HOW this domain RECEIVES (kanban t_04db9228): `mailgun`, `sendgrid`, or `none`.
    ///
    /// This is a separate choice from sending, and switching it changes nothing about how the
    /// domain sends. `sendgrid` mints this domain's inbound token if it does not have one yet, and
    /// the setup page then shows the Inbound Parse URL to paste into SendGrid. An unrecognised value
    /// is refused rather than stored: a receiving provider with no receiver behind it would leave
    /// every delivery unanswered while the panel claimed the domain was set up.
    #[serde(default)]
    pub receiving_provider: Option<String>,
    /// Point the domain at a DIFFERENT saved key (from `GET /private-email/keys`).
    #[serde(default)]
    pub api_key_id: Option<Uuid>,
    /// Rotate or CLEAR the domain's Mailgun webhook signing key (kanban t_fbd607a7). Three distinct
    /// states, and the column is the receiver's only credential, so the difference matters:
    ///
    ///   * **omitted**            -> the stored key is left alone (`None` — this is the only field in
    ///     this request that can clear something, so "absent" must not mean "clear");
    ///   * **`""` (empty)**       -> left alone as well: an empty string means "leave as is"
    ///     everywhere else in this API, and a panel that posts every field on save must not be able
    ///     to disarm the receiver by submitting the empty input box;
    ///   * **`null` (explicit)**  -> CLEARED (the column goes to SQL NULL, i.e. the domain is back to
    ///     "no key configured" and every inbound delivery for it is refused 401). The panel's "Turn
    ///     inbound off" button is the only caller that sends this;
    ///   * **a value**            -> sealed with `crate::secret_box::seal(account_id, key)` and
    ///     stored as `enc:v1:` ciphertext; the panel shows presence + length afterwards, never the
    ///     value.
    #[serde(default, deserialize_with = "some_or_none")]
    pub webhook_signing_key: Option<Option<String>>,
}

/// Distinguish "the field is absent" (`None`) from "the field is explicitly JSON `null`"
/// (`Some(None)`) on an `Option<Option<T>>` field. Plain serde folds both into `None`, which would
/// make it impossible to tell "leave the stored credential alone" from "clear it" — and on this
/// request those are opposite outcomes for the inbound receiver.
fn some_or_none<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(de).map(Some)
}

#[cfg(test)]
mod webhook_key_request_tests {
    use super::UpdateDomainRequest;

    fn parse(json: &str) -> UpdateDomainRequest {
        serde_json::from_str(json).expect("request must parse")
    }

    /// The whole inbound receiver hangs off this distinction (kanban t_fbd607a7): a PATCH that does
    /// not mention the key must LEAVE it alone, and only an explicit JSON null may clear it.
    #[test]
    fn absent_null_and_empty_are_three_different_answers() {
        let absent = parse(r#"{"catch_all_enabled":false}"#);
        assert!(
            absent.webhook_signing_key.is_none(),
            "absent must mean leave as is"
        );

        let cleared = parse(r#"{"webhook_signing_key":null}"#);
        assert_eq!(
            cleared.webhook_signing_key,
            Some(None),
            "an explicit null must mean clear"
        );

        let blank = parse(r#"{"webhook_signing_key":""}"#);
        assert_eq!(
            blank.webhook_signing_key,
            Some(Some(String::new())),
            "an empty string is preserved as-is so the handler can read it as leave-as-is"
        );

        let rotated = parse(r#"{"webhook_signing_key":"sigkey-abc"}"#);
        assert_eq!(
            rotated.webhook_signing_key,
            Some(Some("sigkey-abc".to_string()))
        );
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateMailboxRequest {
    pub signature: Option<String>,
    pub forwarding_enabled: Option<bool>,
    /// Tick or untick this address as a ticket source (B204). Absent leaves it alone.
    pub creates_ticket: Option<bool>,
    /// The teammate this address is assigned to (kanban t_5f314e9a).
    ///
    /// DELIBERATELY a double option: an ABSENT key means "leave the assignment alone", a `null`
    /// means "give this inbox back to the workspace", and a uuid means "assign it to that person".
    /// A single `Option<Uuid>` cannot say the middle thing, which is how `assigned_user_id` on a
    /// conversation became a set-only door that could never be cleared.
    #[serde(default, deserialize_with = "deserialize_optional_nullable_uuid")]
    pub user_id: Option<Option<Uuid>>,
}

/// `{"user_id": "…"}` -> `Some(Some(id))`, `{"user_id": null}` -> `Some(None)`, absent -> `None`.
fn deserialize_optional_nullable_uuid<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Uuid>::deserialize(deserializer)?;
    Ok(Some(value))
}

// Admin override types

/// Request to set tenant email limit overrides (agency_admin only).
#[derive(Debug, Deserialize)]
pub struct SetTenantEmailLimitsRequest {
    pub max_domains: Option<i32>,
    pub max_mailboxes: Option<i32>,
    pub max_aliases_per_mailbox: Option<i32>,
}

/// Tenant email limits row from DB.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TenantEmailLimits {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub max_domains: Option<i32>,
    pub max_mailboxes: Option<i32>,
    pub max_aliases_per_mailbox: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Combined view: plan defaults + any tenant override.
#[derive(Debug, Clone, Serialize)]
pub struct TenantEmailLimitsResponse {
    pub tenant_id: Uuid,
    pub plan_defaults: PrivateEmailPlanFeatures,
    pub overrides: Option<TenantEmailLimits>,
    pub effective_max_domains: i32,
    pub effective_max_mailboxes: i32,
    pub effective_max_aliases_per_mailbox: i32,
    /// Does the tenant's CURRENT plan sell catch-all? This is the entitlement the inbound router
    /// asks, reported here so an operator reads the same answer the mail path uses instead of
    /// inferring it from a stored switch (kanban t_6b54961b).
    pub sells_catch_all: bool,
    /// Domains on this workspace that still carry the stored catch-all switch.
    pub catch_all_domains_enabled: i64,
    /// Of those, how many are REFUSED right now because the current plan does not sell catch-all.
    /// Non-zero is the ordinary residue of a downgrade: the switch is remembered, the router is
    /// honouring the plan. The operator can see that state instead of having to reason about it.
    pub catch_all_domains_off_plan: i64,
}

// Plan feature limits

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrivateEmailPlanFeatures {
    #[serde(default)]
    pub private_email: bool,
    #[serde(default)]
    pub max_domains: i32,
    #[serde(default)]
    pub max_mailboxes: i32,
    #[serde(default)]
    pub max_aliases_per_mailbox: i32,
    #[serde(default)]
    pub catch_all_enabled: bool,
}
