//! Migration runner — applies versioned `.sql` files in `./migrations` at startup.
//! Mirrors WorkflowSwift's filename-based runner (tracked in `_migrations` table).
//!
//! Each file runs as ONE batch over PostgreSQL's simple query protocol
//! (`sqlx::raw_sql`), which the server executes as a single implicit transaction: the
//! whole file lands, or none of it does. The previous implementation split the file on
//! every `';'` character and ran the fragments as separate statements, downgraded each
//! error to a `warn!` ("may be non-fatal") and then inserted the filename into
//! `_migrations` regardless. Two silent failure modes followed from that: a semicolon
//! inside a comment or a string literal truncated the file, and a genuinely failing
//! statement was swallowed while the file was recorded as applied, so no later deploy
//! ever retried it. Measured in this app (card t_1c8e06d1): `000001_password_resets.sql`
//! is recorded as applied while the table does not exist, so
//! `POST /api/v1/auth/forgot-password` answers HTTP 500 in production (repair card
//! t_47c00655), and `00001_full_schema.sql`'s `loyalty_checkins_daily_cap` unique index
//! never ran because its expression is not IMMUTABLE.
//!
//! A file that fails is NOT recorded, so the next start retries it, and the failure is
//! reported at `error!` level. By default the process then exits non-zero, so a schema
//! that does not match the code fails the deploy instead of being served;
//! `MIGRATIONS_FATAL=0` is the single operational escape hatch (boot anyway, still
//! `error!`). There is deliberately no silent mode.

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

    // (filename, reason) for every file this boot could not apply AND record.
    let mut failures: Vec<(String, String)> = Vec::new();

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
                failures.push((filename.clone(), format!("read failed: {e}")));
                continue;
            }
        };

        tracing::info!("Applying migration: {}", filename);

        // The whole file, one statement batch: no splitting on ';', so a semicolon
        // inside a comment, a string literal or a dollar-quoted body can no longer
        // truncate the migration, and a failure leaves nothing half-applied.
        match sqlx::raw_sql(&sql).execute(pool).await {
            Ok(_) => {
                if let Err(e) = sqlx::query("INSERT INTO _migrations (filename) VALUES ($1)")
                    .bind(&filename)
                    .execute(pool)
                    .await
                {
                    // The SQL landed but the ledger row could not be written. Re-running
                    // the file is harmless (migrations here are idempotent), whereas
                    // recording it blind is exactly how an unapplied migration becomes
                    // invisible. So: loud, and leave it unrecorded.
                    tracing::error!(
                        filename = %filename,
                        error = %e,
                        "Migration applied but could NOT be recorded in _migrations — it will be re-run at the next start"
                    );
                    failures.push((filename.clone(), format!("ledger insert failed: {e}")));
                } else {
                    tracing::info!(filename = %filename, "Migration applied");
                }
            }
            Err(e) => {
                tracing::error!(
                    filename = %filename,
                    error = %e,
                    "MIGRATION FAILED — not recorded in _migrations, so the next start retries it"
                );
                failures.push((filename.clone(), e.to_string()));
            }
        }
    }

    if failures.is_empty() {
        tracing::info!("All migrations applied successfully");
        return;
    }

    for (filename, reason) in &failures {
        tracing::error!(filename = %filename, error = %reason, "migration NOT applied");
    }

    let fatal = std::env::var("MIGRATIONS_FATAL")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);

    if fatal {
        tracing::error!(
            failed = failures.len(),
            "Refusing to serve: {} migration file(s) could not be applied, so the schema does not match this binary. Fix the file(s) and redeploy, or set MIGRATIONS_FATAL=0 to boot anyway.",
            failures.len()
        );
        std::process::exit(1);
    }

    tracing::error!(
        failed = failures.len(),
        "MIGRATIONS_FATAL=0: booting anyway with {} migration file(s) UNAPPLIED — the schema does not match this binary",
        failures.len()
    );
}
