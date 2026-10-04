//! Contact database operations — dedup by email/phone, upsert, list, get.
//!
//! TENANCY (kanban t_369cb159): `contacts` is a SHARED identity pool — one row per human — and
//! `contact_tenants` is the many-to-many boundary that decides which accounts may see it.
//! David, 2026-10-04: "contacts should always be tenant scoped. Tenants should not see other
//! tenants contacts" and "there may be tenants that share leads".
//!
//! The rule, applied by every reader in this module:
//!     an account sees a contact IFF a `contact_tenants` row links them.
//! And by every writer that captures or imports a contact ON BEHALF OF an account:
//!     write that link row, or the lead the tenant just captured is invisible to them.
//! A contact with no link row is visible to nobody — that is the deliberate resting state of the
//! 126 historical rows the backfill could not attribute (see migrations/zz_is10_contact_tenants.sql).

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// Input for upserting a contact.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ContactInput {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub business_name: Option<String>,
    pub website: Option<String>,
}

/// A contact record as returned from queries.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Contact {
    pub id: uuid::Uuid,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub business_name: Option<String>,
    pub website: Option<String>,
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub total_entries: i32,
    pub notes: Option<String>,
    pub notes2: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Link a contact to an account (idempotent).
///
/// This is the write that makes a shared contact visible to the tenant that captured or imported
/// it. `source` is a short provenance label ('entry', 'import', 'checkin', ...).
pub async fn link_contact(
    pool: &PgPool,
    contact_id: &Uuid,
    account_id: &Uuid,
    source: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO contact_tenants (contact_id, account_id, source) VALUES ($1, $2, $3) \
         ON CONFLICT (contact_id, account_id) DO NOTHING",
    )
    .bind(contact_id)
    .bind(account_id)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Does `account_id` see `contact_id`? One query for the callers that only need the verdict.
pub async fn contact_visible_to(
    pool: &PgPool,
    contact_id: &Uuid,
    account_id: &Uuid,
) -> Result<bool, AppError> {
    let visible: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM contact_tenants WHERE contact_id = $1 AND account_id = $2)",
    )
    .bind(contact_id)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(visible)
}

/// Upsert a contact by email (case-insensitive), then phone as fallback.
/// If found, update last_seen_at and increment total_entries.
/// If not found, insert a new record.
/// When `account_id` is Some, the contact is linked to that account (see the module note).
/// Returns the contact id.
pub async fn upsert_contact(
    pool: &PgPool,
    input: &ContactInput,
    account_id: Option<Uuid>,
    source: &str,
) -> Result<Uuid, AppError> {
    // First try to find by email (case-insensitive)
    if let Some(ref email) = input.email {
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM contacts WHERE lower(email) = lower($1)")
                .bind(email)
                .fetch_optional(pool)
                .await?;

        if let Some(id) = existing {
            // Update last_seen_at and increment total_entries
            sqlx::query(
                "UPDATE contacts SET last_seen_at = now(), total_entries = total_entries + 1 WHERE id = $1"
            )
            .bind(id)
            .execute(pool)
            .await?;
            if let Some(account_id) = account_id {
                link_contact(pool, &id, &account_id, source).await?;
            }
            return Ok(id);
        }
    }

    // Fallback: try by phone
    if let Some(ref phone) = input.phone {
        let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM contacts WHERE phone = $1")
            .bind(phone)
            .fetch_optional(pool)
            .await?;

        if let Some(id) = existing {
            sqlx::query(
                "UPDATE contacts SET last_seen_at = now(), total_entries = total_entries + 1 WHERE id = $1"
            )
            .bind(id)
            .execute(pool)
            .await?;
            if let Some(account_id) = account_id {
                link_contact(pool, &id, &account_id, source).await?;
            }
            return Ok(id);
        }
    }

    // Insert new contact
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name, website)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
    )
    .bind(id)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.email)
    .bind(&input.phone)
    .bind(&input.business_name)
    .bind(&input.website)
    .execute(pool)
    .await?;

    if let Some(account_id) = account_id {
        link_contact(pool, &id, &account_id, source).await?;
    }

    Ok(id)
}

/// List the contacts THIS account may see, with pagination and optional search.
pub async fn list_contacts(
    pool: &PgPool,
    account_id: &Uuid,
    limit: i64,
    offset: i64,
    search: Option<&str>,
) -> Result<Vec<Contact>, AppError> {
    let contacts = if let Some(query) = search {
        let pattern = format!("%{}%", query);
        sqlx::query_as::<_, Contact>(
            r#"-- contact_tenants is the visibility boundary (t_369cb159): a caller only ever
               -- sees rows linked to its own account, whatever it searches for.
               SELECT id, first_name, last_name, email, phone, business_name, website,
                      first_seen_at, last_seen_at, total_entries, notes, notes2, created_at
               FROM contacts
               WHERE EXISTS (SELECT 1 FROM contact_tenants ct
                             WHERE ct.contact_id = contacts.id AND ct.account_id = $1)
                 AND (first_name ILIKE $2 OR last_name ILIKE $2 OR email ILIKE $2 OR phone ILIKE $2)
               ORDER BY last_seen_at DESC
               LIMIT $3 OFFSET $4"#,
        )
        .bind(account_id)
        .bind(pattern)
        .bind(limit as i32)
        .bind(offset as i32)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, Contact>(
            r#"SELECT id, first_name, last_name, email, phone, business_name, website,
                      first_seen_at, last_seen_at, total_entries, notes, notes2, created_at
               FROM contacts
               WHERE EXISTS (SELECT 1 FROM contact_tenants ct
                             WHERE ct.contact_id = contacts.id AND ct.account_id = $1)
               ORDER BY last_seen_at DESC
               LIMIT $2 OFFSET $3"#,
        )
        .bind(account_id)
        .bind(limit as i32)
        .bind(offset as i32)
        .fetch_all(pool)
        .await?
    };

    Ok(contacts)
}

/// Get a single contact by ID, but ONLY if this account may see it.
/// A foreign id and an absent id are the same answer (404) — never a re-read of a row the
/// caller does not own.
pub async fn get_contact(
    pool: &PgPool,
    account_id: &Uuid,
    contact_id: &uuid::Uuid,
) -> Result<Contact, AppError> {
    let contact = sqlx::query_as::<_, Contact>(
        r#"SELECT id, first_name, last_name, email, phone, business_name, website,
                  first_seen_at, last_seen_at, total_entries, notes, notes2, created_at
           FROM contacts
           WHERE id = $2
             AND EXISTS (SELECT 1 FROM contact_tenants ct
                         WHERE ct.contact_id = contacts.id AND ct.account_id = $1)"#,
    )
    .bind(account_id)
    .bind(contact_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Contact not found".to_string()))?;

    Ok(contact)
}

/// Create (or adopt) a standalone contact for the calling account — the CSV import path and the
/// console's "new contact".
///
/// If a row with this email already exists in the shared pool it is LINKED to the caller and
/// updated in place: two tenants sharing one person is the point of the many-to-many model, and an
/// import must make an existing shared contact visible to the importer. Otherwise a fresh row is
/// inserted and linked.
pub async fn create_contact(
    pool: &PgPool,
    account_id: &Uuid,
    input: &ContactInput,
    source: &str,
) -> Result<Contact, AppError> {
    // Check for existing contact by email (case-insensitive)
    if let Some(ref email) = input.email {
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM contacts WHERE lower(email) = lower($1)")
                .bind(email)
                .fetch_optional(pool)
                .await?;

        if let Some(id) = existing {
            link_contact(pool, &id, account_id, source).await?;
            return update_contact(pool, account_id, &id, input).await;
        }
    }

    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name, website)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
    )
    .bind(id)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.email)
    .bind(&input.phone)
    .bind(&input.business_name)
    .bind(&input.website)
    .execute(pool)
    .await?;

    link_contact(pool, &id, account_id, source).await?;

    get_contact(pool, account_id, &id).await
}

/// Update an existing contact THIS account may see. A contact it cannot see answers 404 and is
/// never written.
pub async fn update_contact(
    pool: &PgPool,
    account_id: &Uuid,
    contact_id: &Uuid,
    input: &ContactInput,
) -> Result<Contact, AppError> {
    let existing = get_contact(pool, account_id, contact_id).await?;

    // Use input values where provided, fall back to existing values
    let new_first_name = input
        .first_name
        .clone()
        .or_else(|| existing.first_name.clone());
    let new_last_name = input
        .last_name
        .clone()
        .or_else(|| existing.last_name.clone());
    let new_email = input.email.clone().or_else(|| existing.email.clone());
    let new_phone = input.phone.clone().or_else(|| existing.phone.clone());
    let new_business_name = input
        .business_name
        .clone()
        .or_else(|| existing.business_name.clone());
    let new_website = input.website.clone().or_else(|| existing.website.clone());

    // The tenancy check is repeated on the write itself: the row must still be linked to the
    // caller at the moment of the UPDATE (belt and braces — the read above already proved it).
    let result = sqlx::query(
        r#"UPDATE contacts
           SET first_name = $1, last_name = $2, email = $3, phone = $4, business_name = $5, website = $6
           WHERE id = $7
             AND EXISTS (SELECT 1 FROM contact_tenants ct
                         WHERE ct.contact_id = contacts.id AND ct.account_id = $8)"#,
    )
    .bind(new_first_name)
    .bind(new_last_name)
    .bind(new_email)
    .bind(new_phone)
    .bind(new_business_name)
    .bind(new_website)
    .bind(contact_id)
    .bind(account_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Contact not found".to_string()));
    }

    get_contact(pool, account_id, contact_id).await
}

/// Delete a contact, tenant-correctly.
///
/// `contacts` is shared, so deleting the row outright would destroy another tenant's view of a
/// person they still own. The caller's own link is what goes; the identity row is only removed
/// once NO account is linked to it any more (then it is truly orphaned).
/// Returns false when the caller had no link (nothing to delete, or already gone).
pub async fn delete_contact(
    pool: &PgPool,
    account_id: &Uuid,
    contact_id: &Uuid,
) -> Result<bool, AppError> {
    let unlinked =
        sqlx::query("DELETE FROM contact_tenants WHERE contact_id = $1 AND account_id = $2")
            .bind(contact_id)
            .bind(account_id)
            .execute(pool)
            .await?;

    if unlinked.rows_affected() == 0 {
        return Ok(false);
    }

    // Only when the row is now linked to nobody at all is it safe to drop the shared identity.
    sqlx::query(
        "DELETE FROM contacts WHERE id = $1 \
         AND NOT EXISTS (SELECT 1 FROM contact_tenants WHERE contact_id = $1)",
    )
    .bind(contact_id)
    .execute(pool)
    .await?;

    Ok(true)
}
