//! Shared application state passed to all handlers.

use crate::config::AppConfig;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub config: Arc<AppConfig>,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub async fn new(config: &Arc<AppConfig>) -> Result<Self, anyhow::Error> {
        let pool = PgPoolOptions::new()
            .min_connections(config.db_min_connections)
            .max_connections(config.db_max_connections)
            .connect(&config.database_url)
            .await?;

        // Run database migrations (filename-based, idempotent)
        crate::db::migrations::run_migrations(&pool).await;

        // Posture line: a missing master key means BYOK writes fail closed by design. Say so
        // in the boot log instead of discovering it on the first customer write.
        if crate::security::provider_key_crypto::is_configured() {
            tracing::info!("Provider key encryption: enabled (AES-256 at rest, enc:v1 format)");
        } else {
            tracing::error!(
                "Provider key encryption: DISABLED — PROVIDER_KEY_ENC_SECRET missing/short; \
                 BYOK writes fail closed (a plaintext credential is never stored)"
            );
        }

        // The shared outbound client follows NO redirects. `reqwest`'s default follows up to 10,
        // so a destination that passed the webhook security gate (`security::webhook_security`)
        // could 30x to `169.254.169.254` or `127.0.0.1` and be a free hop past it — the same
        // policy the test-webhook route set on its own client (kanban t_016c839c). No first-party
        // destination we talk to (CoreSwift, Telnyx, the LLM providers) needs a redirect.
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("IncentiveSwift/0.1.0")
            .build()?;

        Ok(Self {
            db: pool,
            config: Arc::clone(config),
            http_client,
        })
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish()
    }
}
