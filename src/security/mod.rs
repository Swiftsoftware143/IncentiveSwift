//! Security middleware — headers, auth.
//!
//! Request-rate limiting is deliberately NOT here: it is nginx's job (`limit_req zone=api` on
//! this app's `/api/` locations, keyed on Cloudflare's `CF-Connecting-IP`), because the app has
//! no visitor IP to key on — nginx sends `X-Real-IP=$remote_addr` (the Cloudflare edge) and
//! `axum::serve` never populates `ConnectInfo<SocketAddr>`. See kanban t_636d4979.
//!
//! ONE exception, corrected 2026-10-25: CREDENTIAL guessing is counted in-process by
//! [`crate::login_throttle`]. nginx's 20 r/s with `burst=40` is a flood control — a guesser
//! pacing 5/second stays under it for ever — so failed sign-ins are counted per client
//! address there. The address comes from `X-Real-IP`, which `nginx.conf`'s `real_ip` block
//! rewrites from `CF-Connecting-IP`, so it is the VISITOR, not the Cloudflare edge (this
//! file previously said the edge).

pub mod auth;
pub mod body_rejection;
pub mod display_name;
pub mod email_addr;
pub mod headers;
pub mod jwt;
pub mod path_rejection;
pub mod probe_addr;
pub mod provider_key_crypto;
pub mod public_projection;
pub mod route_policy;
pub mod webhook_security;
