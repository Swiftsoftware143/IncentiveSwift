//! Application configuration loaded from environment variables at startup.

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub database_url: String,
    pub redis_url: String,
    pub host: String,
    pub port: u16,
    pub aes_encryption_key: String,
    pub db_min_connections: u32,
    pub jwt_secret: String,
    pub db_max_connections: u32,
    pub internal_sync_key: String,
    pub coreswift_url: String,
    pub workflowswift_url: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        // Try loading .env file, ignore if not found
        let _ = dotenvy::dotenv();

        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .map_err(|_| ConfigError::MissingVar("DATABASE_URL".to_string()))?,

            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8082".to_string())
                .parse()
                .map_err(|e| ConfigError::InvalidValue(format!("PORT: {}", e)))?,
            aes_encryption_key: std::env::var("AES_ENCRYPTION_KEY")
                .map_err(|_| ConfigError::MissingVar("AES_ENCRYPTION_KEY".to_string()))?,
            db_min_connections: std::env::var("DB_MIN_CONNECTIONS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .map_err(|e| ConfigError::InvalidValue(format!("DB_MIN_CONNECTIONS: {}", e)))?,
            jwt_secret: std::env::var("JWT_SECRET")
                .expect("JWT_SECRET is required"),

            internal_sync_key: std::env::var("INTERNAL_SYNC_KEY")
                .unwrap_or_else(|_| String::new()),

            coreswift_url: std::env::var("CORESWIFT_URL")
                .unwrap_or_else(|_| "http://localhost:8084".to_string()),

            workflowswift_url: std::env::var("WORKFLOWSWIFT_URL")
                .unwrap_or_else(|_| "http://localhost:8085".to_string()),

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
