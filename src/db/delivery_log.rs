//! Delivery log database operations — audit trail for webhook/API pushes.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// A delivery log entry.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct DeliveryLogEntry {
    pub id: uuid::Uuid,
    pub entry_id: uuid::Uuid,
    pub method: String,
    pub target: String,
    pub success: bool,
    pub response_code: Option<i32>,
    pub response_body: Option<String>,
    pub attempted_at: chrono::DateTime<chrono::Utc>,
}

/// Log a delivery attempt.
pub async fn log_delivery(
    pool: &PgPool,
    entry_id: &Uuid,
    method: &str,
    target: &str,
    success: bool,
    response_code: Option<i32>,
    response_body: Option<String>,
) -> Result<(), AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_code, response_body)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#
    )
    .bind(id)
    .bind(entry_id)
    .bind(method)
    .bind(target)
    .bind(success)
    .bind(response_code)
    .bind(&response_body)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get delivery log entries for an entry.
#[allow(dead_code)]
pub async fn get_delivery_log(
    pool: &PgPool,
    entry_id: &Uuid,
) -> Result<Vec<DeliveryLogEntry>, AppError> {
    let log = sqlx::query_as::<_, DeliveryLogEntry>(
        r#"SELECT id, entry_id, method, target, success, response_code, response_body, attempted_at
           FROM delivery_log WHERE entry_id = $1
           ORDER BY attempted_at DESC"#,
    )
    .bind(entry_id)
    .fetch_all(pool)
    .await?;

    Ok(log)
}

#[cfg(test)]
mod null_probe_tests {
    //! Opt-in probe against the DEPLOYED schema for the one t_d6e55678 column no route can reach:
    //! `delivery_log.entry_id` is a NULLABLE uuid column decoded into the NON-Option
    //! `DeliveryLogEntry::entry_id`, and `get_delivery_log` — its ONLY reader — has ZERO callers in
    //! the crate (`grep -rn get_delivery_log src/` => this definition only), so the card's "one NULL
    //! fails the whole-row decode" claim cannot be observed over HTTP for this one. This executes
    //! both states directly instead of asserting them in prose:
    //!
    //!   PRE  (column still nullable)  -> the INSERT with entry_id = NULL SUCCEEDS -> PANICS here
    //!   POST (t_d6e55678 migration)   -> the INSERT is REFUSED 23502, naming delivery_log.entry_id
    //!
    //! It only ever writes and deletes rows labelled `probe-t_d6e55678`:
    //!   INC_NULLPROBE_DB_TEST=1 DATABASE_URL=... cargo test --lib null_probe -- --nocapture
    use super::*;

    #[tokio::test]
    async fn delivery_log_entry_id_null_is_unrepresentable_and_the_reader_decodes_a_real_row() {
        if std::env::var("INC_NULLPROBE_DB_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = PgPool::connect(&url).await.expect("connect");

        let probe_id = Uuid::new_v4();
        let planted = sqlx::query(
            "INSERT INTO delivery_log (id, entry_id, method, target, success) \
             VALUES ($1, NULL, 'probe-t_d6e55678', 'probe-t_d6e55678', true)",
        )
        .bind(probe_id)
        .execute(&pool)
        .await;

        match planted {
            Ok(_) => {
                // Leave no residue before failing loudly: the state the card calls unrepresentable
                // was accepted, so this schema still has a NULLABLE delivery_log.entry_id.
                let _ = sqlx::query("DELETE FROM delivery_log WHERE id = $1")
                    .bind(probe_id)
                    .execute(&pool)
                    .await;
                panic!(
                    "delivery_log.entry_id accepted a NULL (probe row {}); the t_d6e55678 migration \
                     is NOT applied on this schema",
                    probe_id
                );
            }
            Err(e) => {
                // Assert on the SQLSTATE, not the prose: sqlx's Display carries the message
                // ("null value in column ... violates not-null constraint") but NOT the code, so
                // 23502 has to be read off the typed error.
                let code = match &e {
                    sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
                    _ => None,
                };
                let msg = e.to_string();
                assert_eq!(
                    code.as_deref(),
                    Some("23502"),
                    "expected a NOT NULL violation (SQLSTATE 23502) for delivery_log.entry_id, got: {msg}"
                );
                assert!(
                    msg.contains("entry_id") && msg.contains("delivery_log"),
                    "the refusal must NAME the column and the table, got: {msg}"
                );
                println!(
                    "PROBE POST  : delivery_log.entry_id refused a NULL -> SQLSTATE {} / {}",
                    code.unwrap_or_default(),
                    msg
                );
            }
        }

        // CONTROL (GREEN in both phases): the reader whose field the card is about still decodes a
        // REAL row. Same statement shape, same struct.
        let entry: Option<Uuid> = sqlx::query_scalar("SELECT id FROM entries LIMIT 1")
            .fetch_optional(&pool)
            .await
            .expect("read one entry");
        match entry {
            Some(entry_id) => {
                let log_id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO delivery_log (id, entry_id, method, target, success) \
                     VALUES ($1, $2, 'probe-t_d6e55678', 'probe-t_d6e55678', true)",
                )
                .bind(log_id)
                .bind(entry_id)
                .execute(&pool)
                .await
                .expect("a real delivery_log row must insert");
                let rows = get_delivery_log(&pool, &entry_id)
                    .await
                    .expect("DeliveryLogEntry must decode");
                assert!(
                    rows.iter().any(|r| r.id == log_id),
                    "the reader lost the row it just wrote"
                );
                println!(
                    "PROBE CONTROL: get_delivery_log decoded a real row ({} returned)",
                    rows.len()
                );
                let _ = sqlx::query("DELETE FROM delivery_log WHERE id = $1")
                    .bind(log_id)
                    .execute(&pool)
                    .await;
            }
            None => {
                println!("PROBE CONTROL: skipped (no entries row to attach a delivery_log row to)")
            }
        }
    }
}
