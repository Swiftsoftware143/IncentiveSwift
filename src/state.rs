//! Shared application state passed to all handlers.

use crate::config::AppConfig;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub config: Arc<AppConfig>,
}

impl AppState {
    pub async fn new(config: &AppConfig) -> Result<Self, anyhow::Error> {
        let pool = PgPoolOptions::new()
            .min_connections(config.db_min_connections)
            .max_connections(config.db_max_connections)
            .connect(&config.database_url)
            .await?;

        Ok(Self {
            db: pool,
            config: Arc::new(config.clone()),
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
