//! Contact database operations — dedup by email/phone, upsert, list, get.

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
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub total_entries: i32,
    pub notes: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Upsert a contact by email (case-insensitive), then phone as fallback.
/// If found, update last_seen_at and increment total_entries.
/// If not found, insert a new record.
/// Returns the contact id.
pub async fn upsert_contact(
    pool: &PgPool,
    input: &ContactInput,
) -> Result<Uuid, AppError> {
    // First try to find by email (case-insensitive)
    if let Some(ref email) = input.email {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM contacts WHERE lower(email) = lower($1)"
        )
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
            return Ok(id);
        }
    }

    // Fallback: try by phone
    if let Some(ref phone) = input.phone {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM contacts WHERE phone = $1"
        )
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
            return Ok(id);
        }
    }

    // Insert new contact
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name)
           VALUES ($1, $2, $3, $4, $5, $6)"#
    )
    .bind(id)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.email)
    .bind(&input.phone)
    .bind(&input.business_name)
    .execute(pool)
    .await?;

    Ok(id)
}

/// List contacts with pagination and optional search.
pub async fn list_contacts(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    search: Option<&str>,
) -> Result<Vec<Contact>, AppError> {
    let contacts = if let Some(query) = search {
        let pattern = format!("%{}%", query);
        sqlx::query_as::<_, Contact>(
            r#"SELECT id, first_name, last_name, email, phone, business_name,
                      first_seen_at, last_seen_at, total_entries, notes, created_at
               FROM contacts
               WHERE first_name ILIKE $1 OR last_name ILIKE $1 OR email ILIKE $1 OR phone ILIKE $1
               ORDER BY last_seen_at DESC
               LIMIT $2 OFFSET $3"#,
        )
        .bind(pattern)
        .bind(limit as i32)
        .bind(offset as i32)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, Contact>(
            r#"SELECT id, first_name, last_name, email, phone, business_name,
                      first_seen_at, last_seen_at, total_entries, notes, created_at
               FROM contacts
               ORDER BY last_seen_at DESC
               LIMIT $1 OFFSET $2"#,
        )
        .bind(limit as i32)
        .bind(offset as i32)
        .fetch_all(pool)
        .await?
    };

    Ok(contacts)
}

/// Get a single contact by ID with entry history.
pub async fn get_contact(
    pool: &PgPool,
    contact_id: &uuid::Uuid,
) -> Result<Contact, AppError> {
    let contact = sqlx::query_as::<_, Contact>(
        r#"SELECT id, first_name, last_name, email, phone, business_name,
                  first_seen_at, last_seen_at, total_entries, notes, created_at
           FROM contacts WHERE id = $1"#,
    )
    .bind(contact_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Contact not found".to_string()))?;

    Ok(contact)
}

/// Create a standalone contact (not via entry).
pub async fn create_contact(
    pool: &PgPool,
    input: &ContactInput,
) -> Result<Contact, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name)
           VALUES ($1, $2, $3, $4, $5, $6)"#
    )
    .bind(id)
    .bind(&input.first_name)
    .bind(&input.last_name)
    .bind(&input.email)
    .bind(&input.phone)
    .bind(&input.business_name)
    .execute(pool)
    .await?;

    get_contact(pool, &id).await
}

/// Update an existing contact.
pub async fn update_contact(
    pool: &PgPool,
    contact_id: &Uuid,
    input: &ContactInput,
) -> Result<Contact, AppError> {
    let existing = get_contact(pool, contact_id).await?;

    // Use input values where provided, fall back to existing values
    let new_first_name = input.first_name.clone().or_else(|| existing.first_name.clone());
    let new_last_name = input.last_name.clone().or_else(|| existing.last_name.clone());
    let new_email = input.email.clone().or_else(|| existing.email.clone());
    let new_phone = input.phone.clone().or_else(|| existing.phone.clone());
    let new_business_name = input.business_name.clone().or_else(|| existing.business_name.clone());

    sqlx::query(
        r#"UPDATE contacts
           SET first_name = $1, last_name = $2, email = $3, phone = $4, business_name = $5
           WHERE id = $6"#
    )
    .bind(new_first_name)
    .bind(new_last_name)
    .bind(new_email)
    .bind(new_phone)
    .bind(new_business_name)
    .bind(contact_id)
    .execute(pool)
    .await?;

    get_contact(pool, contact_id).await
}

/// Delete a contact by id.
pub async fn delete_contact(
    pool: &PgPool,
    contact_id: &Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM contacts WHERE id = $1")
        .bind(contact_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}
