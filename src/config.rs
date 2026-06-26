//! Application configuration loaded from environment variables at startup.

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub database_url: String,
    pub supabase_url: String,
    pub supabase_service_key: String,
    pub supabase_anon_key: String,
    pub redis_url: String,
    pub host: String,
    pub port: u16,
    pub aes_encryption_key: String,
    pub db_min_connections: u32,
    pub db_max_connections: u32,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        // Try loading .env file, ignore if not found
        let _ = dotenvy::dotenv();

        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .map_err(|_| ConfigError::MissingVar("DATABASE_URL".to_string()))?,
            supabase_url: std::env::var("SUPABASE_URL")
                .map_err(|_| ConfigError::MissingVar("SUPABASE_URL".to_string()))?,
            supabase_service_key: std::env::var("SUPABASE_SERVICE_KEY")
                .map_err(|_| ConfigError::MissingVar("SUPABASE_SERVICE_KEY".to_string()))?,
            supabase_anon_key: std::env::var("SUPABASE_ANON_KEY")
                .map_err(|_| ConfigError::MissingVar("SUPABASE_ANON_KEY".to_string()))?,
            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .map_err(|e| ConfigError::InvalidValue(format!("PORT: {}", e)))?,
            aes_encryption_key: std::env::var("AES_ENCRYPTION_KEY")
                .map_err(|_| ConfigError::MissingVar("AES_ENCRYPTION_KEY".to_string()))?,
            db_min_connections: std::env::var("DB_MIN_CONNECTIONS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .map_err(|e| ConfigError::InvalidValue(format!("DB_MIN_CONNECTIONS: {}", e)))?,
            db_max_connections: std::env::var("DB_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "20".to_string())
                .parse()
                .map_err(|e| ConfigError::InvalidValue(format!("DB_MAX_CONNECTIONS: {}", e)))?,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Missing environment variable: {0}")]
    MissingVar(String),
    #[error("Invalid value: {0}")]
    InvalidValue(String),
}

impl From<ConfigError> for crate::error::AppError {
    fn from(err: ConfigError) -> Self {
        crate::error::AppError::Internal(err.to_string())
    }
}
