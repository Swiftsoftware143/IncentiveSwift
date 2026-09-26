//! Security middleware — headers, auth.
//!
//! Request-rate limiting is deliberately NOT here: it is nginx's job (`limit_req zone=api` on
//! this app's `/api/` locations, keyed on Cloudflare's `CF-Connecting-IP`), because the app has
//! no visitor IP to key on — nginx sends `X-Real-IP=$remote_addr` (the Cloudflare edge) and
//! `axum::serve` never populates `ConnectInfo<SocketAddr>`. See kanban t_636d4979.

pub mod auth;
pub mod headers;
pub mod jwt;
pub mod provider_key_crypto;
