//! Migration runner — applies versioned `.sql` files in `./migrations` at startup.
//! Mirrors WorkflowSwift's filename-based runner (tracked in `_migrations` table).
//! Idempotent: skips filenames already recorded, splits statements on `;`,
//! and treats per-statement errors as non-fatal warnings.

use sqlx::PgPool;

pub async fn run_migrations(pool: &PgPool) {
    let migration_dir = std::path::Path::new("./migrations");
    if !migration_dir.exists() {
        tracing::warn!("Migrations directory not found at ./migrations");
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(migration_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read migrations directory");
            return;
        }
    }
    .filter_map(|e| e.ok())
    .filter(|e| {
        e.path()
            .extension()
            .map(|ext| ext == "sql")
            .unwrap_or(false)
    })
    .collect();

    entries.sort_by_key(|e| e.file_name());

    if let Err(e) = sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS _migrations (
            id SERIAL PRIMARY KEY,
            filename VARCHAR(255) NOT NULL UNIQUE,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )
    .execute(pool)
    .await
    {
        tracing::error!(error = %e, "Failed to create migrations tracking table");
        return;
    }

    for entry in &entries {
        let filename = entry.file_name().to_string_lossy().to_string();

        let already_applied =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _migrations WHERE filename = $1")
                .bind(&filename)
                .fetch_one(pool)
                .await
                .unwrap_or(0);

        if already_applied > 0 {
            tracing::info!("Migration {} already applied, skipping", filename);
            continue;
        }

        let sql = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(filename = %filename, error = %e, "Failed to read migration file");
                continue;
            }
        };

        tracing::info!("Applying migration: {}", filename);

        for statement in sql.split(';') {
            let trimmed = statement.trim();
            if !trimmed.is_empty() {
                if let Err(e) = sqlx::query(trimmed).execute(pool).await {
                    tracing::warn!(
                        "Migration {} statement warning (may be non-fatal): {}",
                        filename,
                        e
                    );
                }
            }
        }

        if let Err(e) = sqlx::query("INSERT INTO _migrations (filename) VALUES ($1)")
            .bind(&filename)
            .execute(pool)
            .await
        {
            tracing::error!(filename = %filename, error = %e, "Failed to record migration");
        }
    }

    tracing::info!("All migrations applied successfully");
}
