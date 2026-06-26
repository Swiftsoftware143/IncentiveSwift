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

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
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
