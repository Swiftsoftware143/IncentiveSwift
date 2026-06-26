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
    pub fn from_env() -> Result<Self, config::ConfigError> {
        // Try loading .env file, ignore if not found
        let _ = dotenvy::dotenv();

        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .expect("DATABASE_URL must be set"),
            supabase_url: std::env::var("SUPABASE_URL")
                .expect("SUPABASE_URL must be set"),
            supabase_service_key: std::env::var("SUPABASE_SERVICE_KEY")
                .expect("SUPABASE_SERVICE_KEY must be set"),
            supabase_anon_key: std::env::var("SUPABASE_ANON_KEY")
                .expect("SUPABASE_ANON_KEY must be set"),
            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            host: std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .expect("PORT must be a valid number"),
            aes_encryption_key: std::env::var("AES_ENCRYPTION_KEY")
                .expect("AES_ENCRYPTION_KEY must be set"),
            db_min_connections: 5,
            db_max_connections: 20,
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
