//! SMTP sender — credentials come from the DB-backed email provider config
//! (Admin > Settings > Email Provider). Nothing here reads the process environment.
//!
//! ## DECISION (kanban t_578a587b): the PLATFORM `provider = smtp` dial is bounded — twice
//!
//! This file is the PLATFORM arm: the transport an operator picks in Admin → Email Provider → SMTP,
//! reached from `email_provider::deliver`, and therefore from `email::send_email_request` — the
//! app's ONE system-mail path (the capture-time immediate send, every lifecycle stage and the queue
//! ticker). **ARM A** of the card: the same ONE bound the TENANT arm already carries
//! ([`crate::delivery::sender::TENANT_SMTP_DEADLINE`]), applied twice — `.timeout(..)` on the
//! builder for the TCP CONNECT, plus a `tokio::time::timeout` around the SMTP dialogue through the
//! SHARED [`crate::delivery::sender::send_with_deadline`] helper — so this arm's `last_error`, the
//! tick's log line and the panel's "Not sent. …" all read exactly like the tenant arm's:
//! `<host:port> did not answer within 10s`.
//!
//! **Why a bound at all — and why the dialogue half is not optional.** Read in the vendored lettre
//! 0.11.23 source, the builder's `.timeout(..)` reaches `AsyncNetworkStream::connect_tokio1`, which
//! wraps ONLY `socket.connect(addr)`; the async stream it then builds carries no read/write timeout,
//! so `conn.read_response().await` is unbounded. Before this change NEITHER half existed here:
//! `grep -n "timeout" src/smtp.rs` returned nothing. **Measured live 2026-10-02** (while building
//! the parent card t_f56f4a79's SMTP sink): a server that accepted the TCP connection and answered
//! EHLO with a lone `250-…` CONTINUATION — a line a real client waits on for ever — parked this
//! send past 160 s, and because the queue ticker's flush is SEQUENTIAL (kanban t_96695538) that one
//! row sat in front of every other tenant's queued mail for the rest of the run. Unbounded, not the
//! "10 s per row" t_96695538 bounds.
//!
//! **Why 10 s.** The same number as the tenant arm and this app's dominant outbound bound — see
//! [`crate::delivery::sender::TENANT_SMTP_DEADLINE`]'s doc for the census of 10 s call sites. A mail
//! server that is up answers a banner in well under a second, so 10 s is generous for a real relay
//! and is the difference between "the send failed, retryable" and "the queue is parked for ever".
//! The two constants are pinned equal by a unit test (`the_platform_bound_is_the_tenant_bound`) so
//! the two arms cannot drift apart.
//!
//! **Arms rejected** (recorded, per the card). B — a different number: no measurement argues one;
//! a longer bound only buys a slower park, a shorter one abandons a slow-but-healthy relay. C —
//! nothing, documented here: the defect the card measured would stand, and because the flush is
//! sequential and this arm serves every tenant with no mail server of its own, ONE stalled platform
//! provider parks the whole fleet's queued mail with no operator-visible end. Note what is still
//! NOT covered: DNS (`lookup_host`) and the SSRF gate's own lookup run before this dial and are
//! bounded elsewhere.

use crate::delivery::sender::send_with_deadline;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_email: String,
    pub from_name: Option<String>,
    pub encryption: Option<String>, // "tls" or "starttls"
}

/// How long ONE PLATFORM `provider = smtp` dial may take, end to end: TCP connect plus the SMTP
/// dialogue (banner / EHLO / AUTH / MAIL / RCPT / DATA). The same number as the tenant arm's
/// [`crate::delivery::sender::TENANT_SMTP_DEADLINE`] — one number for every SMTP dial this app
/// makes — and pinned equal by `the_platform_bound_is_the_tenant_bound`. The decision and the
/// measurement behind it are in this module's docs.
pub const PLATFORM_SMTP_DEADLINE: Duration = Duration::from_secs(10);

/// Send an email via SMTP using the provided config. Returns Ok(()) on success.
pub async fn send_via_smtp(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    html: Option<&str>,
) -> Result<(), String> {
    send_via_smtp_with_deadline(config, to, subject, body, html, PLATFORM_SMTP_DEADLINE).await
}

/// The body of [`send_via_smtp`], with the bound injectable so a unit test can shorten it (the
/// dial it guards is real, so the bound is the only thing that may be faked).
async fn send_via_smtp_with_deadline(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    html: Option<&str>,
    deadline: Duration,
) -> Result<(), String> {
    use lettre::message::{MultiPart, SinglePart};
    use lettre::{
        transport::smtp::authentication::Credentials, AsyncSmtpTransport, Message, Tokio1Executor,
    };

    let from_name = config.from_name.as_deref().unwrap_or("IncentiveSwift");
    let from_addr = format!("{} <{}>", from_name, config.from_email);

    let builder = Message::builder()
        .from(
            from_addr
                .parse()
                .map_err(|e| format!("Invalid from address: {}", e))?,
        )
        .to(to.parse().map_err(|e| {
            format!(
                "{} {}",
                crate::delivery::sender::INVALID_RECIPIENT_PREFIX,
                e
            )
        })?)
        .subject(subject);

    let email = match html.filter(|h| !h.is_empty()) {
        Some(h) => builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(body.to_string()))
                    .singlepart(SinglePart::html(h.to_string())),
            )
            .map_err(|e| format!("Failed to build email: {}", e))?,
        None => builder
            .multipart(MultiPart::alternative().singlepart(SinglePart::plain(body.to_string())))
            .map_err(|e| format!("Failed to build email: {}", e))?,
    };

    // Credentials are attached ONLY when a username is actually configured.
    //
    // Measured 2026-10-01: with an empty username the transport still NEGOTIATES auth, and a relay that
    // wants none answers "No compatible authentication mechanism was found" — which is exactly how the
    // credential email died the first time this path was exercised end to end. An unauthenticated relay
    // is an ordinary mail server (an internal one, or a local sink), so asking it to authenticate makes
    // the panel's "SMTP (any mail server)" label false. Same fix as FunnelSwift's smtp.rs.
    let has_auth = !config.username.trim().is_empty();
    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let base = match config.encryption.as_deref() {
        Some("tls") => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
            .map_err(|e| format!("Failed to create TLS SMTP transport: {}", e))?,
        // PLAIN SMTP, no TLS at all. Added 2026-10-01 for the same reason FunnelSwift needed it: the
        // panel offers this field as "SMTP (any mail server)", and an internal relay — or a local
        // sink — without TLS is a perfectly ordinary mail server. Refusing it made that label false,
        // and it left the credential proof unable to capture what the app actually sent.
        Some("none") => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host),
        _ => {
            // STARTTLS (default)
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| format!("Failed to create STARTTLS SMTP transport: {}", e))?
        }
    };

    // Every arm above yields the same builder type, so the port and the OPTIONAL credentials are
    // applied once, here.
    let base = base.port(config.port);
    let base = if has_auth {
        base.credentials(creds)
    } else {
        base
    };
    // BOUND 1 of 2 — the TCP CONNECT. lettre uses this knob ONLY for `socket.connect(addr)`
    // (`client::async_net::try_connect` wraps it in `tokio::time::timeout`); it does NOT set a
    // read/write timeout on the async stream, so it cannot bound a server that accepts and then says
    // nothing. That half is BOUND 2, below. See this module's DECISION block (kanban t_578a587b).
    let mailer = base.timeout(Some(deadline)).build();

    // BOUND 2 of 2 — the SMTP DIALOGUE (banner / EHLO / AUTH / MAIL / RCPT / DATA), through the
    // SAME helper the tenant arm uses, so this arm's refusal — and therefore the queue row's
    // `last_error`, the tick's log line and the panel's "Not sent. …" — reads exactly like the
    // tenant arm's: `<host:port> did not answer within 10s …`.
    let label = format!("{}:{}", config.host, config.port);
    send_with_deadline(mailer, email, &label, deadline).await
}

#[cfg(test)]
mod platform_deadline_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// A PLAIN (`encryption = "none"`) platform config pointed at a test socket.
    fn config(port: u16) -> SmtpConfig {
        SmtpConfig {
            host: "127.0.0.1".to_string(),
            port,
            username: String::new(),
            password: String::new(),
            from_email: "probe@incentiveswift.com".to_string(),
            from_name: None,
            encryption: Some("none".to_string()),
        }
    }

    async fn silent_sink() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept, hold the socket open, never write — the "accepts and says nothing" sink.
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(stream);
            }
        });
        port
    }

    /// The case this card exists for (kanban t_578a587b): the PLATFORM arm against a server that
    /// ACCEPTS the TCP connection and then sends nothing — no banner, no RST. Without BOUND 2 this
    /// send never returns, so the probe's own `expect_err` would never be reached (the test would
    /// hang), and with the wrapper removed the assertion below goes red on a transport error.
    #[tokio::test]
    async fn a_platform_dial_that_is_never_answered_is_abandoned_at_the_bound() {
        let port = silent_sink().await;
        let started = std::time::Instant::now();
        let err = send_via_smtp_with_deadline(
            &config(port),
            "probe@probe-target.com",
            "probe",
            "probe",
            None,
            Duration::from_millis(400),
        )
        .await
        .expect_err("a silent server is not a delivered message");
        assert!(
            err.contains("did not answer within"),
            "the refusal must name the bound, got: {err}"
        );
        assert!(
            err.contains(&format!("127.0.0.1:{port}")),
            "the refusal must name WHICH server was abandoned, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the dial was not bounded: {:?}",
            started.elapsed()
        );
    }

    /// The EXACT shape measured on 2026-10-02 (parent card t_f56f4a79's sink): a banner, then EHLO
    /// answered with a lone `250-…` CONTINUATION and nothing more. A real client loops until it sees
    /// a line WITHOUT the hyphen, so this is an unbounded wait — not a transport error.
    #[tokio::test]
    async fn a_lone_250_continuation_on_ehlo_is_abandoned_at_the_bound() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let _ = w.write_all(b"220 probe-smtp-sink ESMTP\r\n").await;
                let _ = w.flush().await;
                let mut lines = BufReader::new(r).lines();
                let _ = lines.next_line().await; // the client's EHLO
                let _ = w.write_all(b"250-probe-smtp-sink\r\n").await;
                let _ = w.flush().await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        let started = std::time::Instant::now();
        let err = send_via_smtp_with_deadline(
            &config(port),
            "probe@probe-target.com",
            "probe",
            "probe",
            None,
            Duration::from_millis(400),
        )
        .await
        .expect_err("a server stuck on a continuation is not a delivered message");
        assert!(
            err.contains("did not answer within"),
            "the refusal must name the bound, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the dialogue was not bounded: {:?}",
            started.elapsed()
        );
    }

    /// Anti-drift: the platform arm and the tenant arm are one number by decision. If someone
    /// changes one of them, this goes red instead of the two arms silently disagreeing.
    #[test]
    fn the_platform_bound_is_the_tenant_bound() {
        assert_eq!(
            PLATFORM_SMTP_DEADLINE,
            crate::delivery::sender::TENANT_SMTP_DEADLINE
        );
    }
}
