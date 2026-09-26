//! Request-body read deadline (kanban t_af70c0ca).
//!
//! IncentiveSwift's public surfaces are the ones a stranger can reach: the loyalty/campaign
//! read-and-redeem routes (`POST /api/v1/loyalty/checkin`, `/api/v1/loyalty/online/visit`,
//! `/api/v1/loyalty/online/share`), the entry capture routes (`POST /api/v1/entries`,
//! `POST /api/v1/raffles/:slug/enter`, `POST /api/v1/campaigns/:slug/spin`,
//! `/api/v1/campaigns/:slug/long-form-qualifier`), the Surface/embed submit routes, the SMS
//! receiver `POST /api/v1/channels/inbound`, the billing webhooks
//! (`POST /api/v1/webhooks/stripe`, `/api/v1/webhooks/paypal`), the entry-webhook tester
//! `POST /api/v1/campaigns/test-webhook` and the auth routes (`POST /api/v1/auth/login`,
//! `/api/v1/auth/forgot-password`, `/api/v1/auth/reset-password`) — and every one of them reads a
//! request body.
//!
//! ## What was actually wrong here (measured, not grepped)
//!
//! The card was raised from `grep -rl "body_read_deadline" src/` returning 0 files, and its premise
//! was that a client could send a request head with a valid `Content-Length` and then stop, parking
//! the task, the connection and a partially-read body buffer "for ever". **That premise is
//! measurably false on this app**: the router has carried a whole-request
//! `.layer(TimeoutLayer::new(Duration::from_secs(30)))` since before this card, so a stalled body is
//! answered `408` at t+30.0 s (measured live on the deployed pre-change binary,
//! `audits/t_af70c0ca/10-before.txt`).
//!
//! What IS wrong is the KIND of bound, and it is the exact anti-pattern the fleet rejected when the
//! CoreSwift-CRM arm was built (t_59745689):
//!
//! * It is a budget over the WHOLE request, not over the body's arrival. A request whose body
//!   arrived instantly is still answered `408` at t+30 s while its handler is legitimately still
//!   working — measured live: a `POST /api/v1/loyalty/checkin` held on a DB row lock for 40 s was
//!   answered `408` at t+30.0 s with the work dropped, while the identical request on the
//!   post-change binary is allowed to finish. For an entry-capture receiver that is a LOST ENTRY,
//!   not a retried request.
//! * Its `408` carries none of this fleet's 408 contract: no `Connection: close` (the declared body
//!   was never consumed, so the connection must not be reused), no bound named in the body, no
//!   release line in the app log (nothing an operator can see), and no way to configure it.
//! * It is the only whole-request budget in the fleet: WorkflowSwift (t_e7cba83e), ADASwift
//!   (t_b3d626ed), FunnelSwift (t_488a19d4) and CoreSwift-CRM (t_59745689) all bound the body's
//!   ARRIVAL and nothing else.
//!
//! ## Why this middleware does not pre-read the body (the one deliberate difference from
//! ## `CoreSwift-CRM/src/body_deadline.rs`)
//!
//! The CoreSwift arm reads the body itself — `Bytes::from_request` under a `tokio::time::timeout` —
//! and hands the completed bytes to the inner service. That is safe there because **CoreSwift guards
//! with `auth_middleware`**, so mounting the layer first (innermost) still leaves the credential
//! check outside it and an unauthenticated `401` never waits for a body.
//!
//! **IncentiveSwift does not guard that way.** It has no auth middleware at all beyond the
//! path-scoped `security::auth::admin_guard` for `/api/v1/admin/*`; every other protected route
//! checks its credential with an `AuthenticatedUser` extractor *inside the handler*. Measured: of
//! the 70 handlers that take both a credential and a body, **70 take the credential FIRST and 0 take
//! the body first** (`scope-census.txt`). A middleware that pre-reads the body runs *outside* those
//! extractors, so on all 70 routes it would buffer a declared body — up to axum's 2 MiB — before the
//! credential was ever checked, and would answer `408` at the bound instead of the `401` that
//! arrives at t+0.0 s today. That is precisely what this card forbids: *"an unauthenticated 401 must
//! not start waiting for a body, and a declared body must not be buffered before the credential is
//! checked"*.
//!
//! So the bound is expressed the other way round, with the same semantics: the request is handed to
//! the inner service with its ORIGINAL body, unchanged, and the layer races that call against a
//! timer. If the inner service answers first — a `401`, a `400`, an over-limit `413`, anything — the
//! answer is returned at once and the body is never touched. If the body has finished arriving by
//! the time the timer fires, the layer simply waits for the handler, because a request whose body
//! HAS arrived is not unbounded and a handler's own work is not this layer's business to time out.
//! The `408` is therefore produced on exactly one condition: **the body had not finished arriving by
//! the deadline**, which is the CoreSwift arm's behaviour and the fleet's contract. What is not
//! reproduced is only the incidental pre-buffering.

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::warn;

/// Default body-read deadline: how long the body of a request that carries one may take to arrive
/// before the request is answered `408` and its task, connection and partially-read body buffer are
/// released. Override with `BODY_READ_DEADLINE_SECS` (clamped to `5..=300` in `config.rs`, so
/// neither a typo nor a fat finger can shed real traffic).
///
/// 30 s matches the accidental budget this module replaces, so the resource bound is not loosened,
/// and it is orders of magnitude above the time a real body on these routes takes — an entry
/// capture or a Telnyx event is kilobytes over a same-region link — while still generous to a slow
/// sender: a full 2 MiB body (axum's default `DefaultBodyLimit`, which this app does not raise
/// anywhere) may arrive as slowly as ~70 KiB/s and complete inside it.
pub const DEFAULT_BODY_READ_DEADLINE_SECS: u64 = 30;

/// Lower clamp for `BODY_READ_DEADLINE_SECS`. Below this a legitimate slow sender would start
/// losing entries, so the value is refused rather than honoured.
pub const MIN_BODY_READ_DEADLINE_SECS: u64 = 5;

/// Upper clamp for `BODY_READ_DEADLINE_SECS`: above this the "bound" stops being one.
pub const MAX_BODY_READ_DEADLINE_SECS: u64 = 300;

/// How long a request body may take to arrive, as configured.
///
/// Its own type so the middleware can be mounted (and unit-tested) without an `AppState` — and so
/// the number it enforces is the number the boot log printed, never re-derived per request.
#[derive(Clone, Copy, Debug)]
pub struct BodyReadDeadline(std::time::Duration);

impl BodyReadDeadline {
    /// From the configured seconds. Clamping lives in `config.rs`; this is a plain carrier.
    pub fn from_secs(seconds: u64) -> Self {
        Self(std::time::Duration::from_secs(seconds))
    }

    /// The deadline as a duration.
    pub fn duration(self) -> std::time::Duration {
        self.0
    }
}

/// The `408` a request whose body never finished arriving is answered with.
///
/// Carries the deadline so a client log says which bound was hit, and `Connection: close` because
/// the declared body was never consumed: this connection cannot be reused for another request.
fn body_deadline_response(deadline: std::time::Duration) -> Response {
    Response::builder()
        .status(StatusCode::REQUEST_TIMEOUT)
        .header("Connection", "close")
        .body(Body::from(format!(
            "{{\"error\":\"Request body was not received in time. Retry.\",\"status\":408,\
             \"body_deadline_seconds\":{}}}",
            deadline.as_secs()
        )))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::REQUEST_TIMEOUT)
                .header("Connection", "close")
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty()))
        })
}

/// A request body that records whether it ever reached its end.
///
/// The layer needs exactly one bit out of the body — has it finished arriving? — and the only way to
/// observe that without consuming it is to sit in the poll path. Consuming it is what this module
/// deliberately avoids (see the module docs), so the layer wraps the body and watches it being read
/// by whoever actually wants it: the handler's own extractor.
struct ArrivalTrackingBody {
    inner: Body,
    arrived: Arc<AtomicBool>,
}

impl HttpBody for ArrivalTrackingBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            // The last frame, or a read error: either way the body is no longer something a client
            // is still holding us open for, so the deadline must stand down.
            Poll::Ready(None) => {
                this.arrived.store(true, Ordering::SeqCst);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                this.arrived.store(true, Ordering::SeqCst);
                Poll::Ready(Some(Err(err)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Middleware: bound how long a request BODY may take to arrive, and answer `408` if it has not
/// finished within [`BodyReadDeadline`] of the headers.
///
/// **Why a body deadline and not a handler budget.** These routes do their own credential work
/// *after* the body has been read: `POST /api/v1/auth/login` looks the account up and verifies a
/// password hash, the `/api/v1/channels/inbound` receiver resolves the sender, the billing webhooks
/// verify a signature, `POST /api/v1/loyalty/checkin` upserts a contact and debits points. A
/// wall-clock budget over those routes has to be sized for that work plus everything the handler
/// dispatches, and elapsing it drops work that was legitimately in progress — a LOST ENTRY, not a
/// retried request. This middleware bounds only the thing that is actually unbounded: a client that
/// sends a request head and then stops. A handler that legitimately takes seconds is untouched,
/// because the deadline is over by the time it runs.
///
/// **The deadline is total, measured from the headers** — not a per-chunk idle timeout that resets
/// on every frame (`tower_http::timeout::RequestBodyTimeoutLayer` works that way, and a client that
/// dribbles one byte every N-1 seconds defeats it).
///
/// **What a legitimately slow sender gets.** A body that *is* arriving is read at full speed; the
/// deadline only fires on one that has stopped. If a body really does exceed the deadline the request
/// is answered `408` and closed, and the senders that matter (Telnyx, Stripe/PayPal, the embed
/// widgets, the entry webhook) retry — at that point the sender had stopped mid-body, so the event
/// was already undeliverable.
///
/// **The body is handed on untouched**, so the handler's own extractor (`Json`, `Bytes`,
/// `Multipart`) sees exactly the bytes the client sent, with the same headers, the same
/// `DefaultBodyLimit` and the same `413` for an over-limit body — because that extractor is the only
/// thing that ever reads it.
pub async fn body_read_deadline_middleware(
    State(deadline): State<BodyReadDeadline>,
    request: Request,
    next: Next,
) -> Response {
    let deadline = deadline.duration();

    // Scope (the same expression the FunnelSwift arm of this bound uses, t_488a19d4): the deadline
    // engages only for a request that DECLARES a body on a method that can carry one. Everything
    // else is handed to the inner service with its original body, untouched — so a route that reads
    // no body (a GET, a bodyless POST) is not changed by this layer at all: with no declared body
    // there is nothing that can be waited for, so there is nothing to bound and no behaviour to
    // alter. That is what makes the boot log's "on every route that reads a body" a measurement and
    // not a claim, and it is why this app's single flat router can carry the layer without the
    // contract of its GET-only routes changing (proven live in both phases: a declared-but-absent
    // body on `GET /api/v1/health` is answered `200` at t+0.0 s).
    if !declares_a_body(request.method(), request.headers()) {
        return next.run(request).await;
    }

    let method = request.method().clone();
    let uri = request.uri().clone();

    let (parts, body) = request.into_parts();
    let arrived = Arc::new(AtomicBool::new(false));
    let tracked = Body::new(ArrivalTrackingBody {
        inner: body,
        arrived: arrived.clone(),
    });

    let inner = next.run(Request::from_parts(parts, tracked));
    tokio::pin!(inner);

    // `biased` so a handler that has already produced its answer always wins the tie: the deadline
    // can only ever ADD a `408` to a request that has not been answered, never replace one.
    tokio::select! {
        biased;
        response = &mut inner => response,
        _ = tokio::time::sleep(deadline) => {
            if arrived.load(Ordering::SeqCst) {
                // The body finished arriving inside the bound. Whatever is still running is the
                // handler's own work — not an unarrived body — so it is not this layer's business to
                // cut it off: wait for it. (Before this module, a request in exactly this position
                // was answered `408` at t+30 s by the app's accidental whole-request `TimeoutLayer`
                // and its work was dropped.)
                inner.await
            } else {
                warn!(
                    "request body not received within {:?} (stalled body) — answered 408 for {} {}",
                    deadline, method, uri
                );
                body_deadline_response(deadline)
            }
        }
    }
}

/// Whether the request declares a body a handler could wait for: `Content-Length` greater than
/// zero, or `Transfer-Encoding`, on a method that can carry a body.
///
/// A `Content-Length: 0` declares no body and is answered without a wait, and neither is a GET:
/// there is no declared body for a handler to block on, so bounding one would only invent a new
/// failure mode for requests that work today.
fn declares_a_body(method: &Method, headers: &HeaderMap) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return false;
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    declared > 0 || headers.contains_key(header::TRANSFER_ENCODING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::Router;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // ── body-read deadline (kanban t_af70c0ca) ──────────────────────────────────────────────
    //
    // Six properties, and they are the ones a later refactor must not break: the deadline fires on a
    // body that never arrives; a body that does arrive reaches the handler byte for byte (the entry,
    // checkin and webhook handlers parse the raw JSON, so anything else silently breaks lead
    // capture); the middleware does not widen the limit on how much a single request may buffer; a
    // request that declares NO body is not waited for at all; a handler that answers WITHOUT reading
    // its body answers at once (this app checks its credentials in the handler, so a `401` must never
    // wait for a body); and a handler that has finished reading its body is not cut off (the
    // whole-request budget this module replaces did exactly that).
    //
    // Driven over a real socket rather than through `tower::ServiceExt::oneshot`: it needs no
    // dev-dependency this app does not already carry, and it exercises the actual hyper path a
    // stranger reaches (the same shape the live probe uses).

    /// The production mount shape: a route whose handler reads the body, a route whose handler
    /// answers without reading it at all (the shape this app's credential-first handlers produce —
    /// their `AuthenticatedUser` extractor rejects before the body extractor would run, so nothing
    /// ever polls the body), a route that answers long after the body arrived, plus a bodyless GET
    /// route — all wrapped by the layer.
    fn app(deadline: Duration) -> Router {
        Router::new()
            .route("/body", post(|body: Bytes| async move { body }))
            .route(
                "/authorized",
                post(|| async { (StatusCode::UNAUTHORIZED, "no credential") }),
            )
            .route(
                "/slow",
                post(|_body: Bytes| async {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    "done"
                }),
            )
            .route("/nobody", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                BodyReadDeadline(deadline),
                body_read_deadline_middleware,
            ))
    }

    /// Serve `router` on an ephemeral port and hand back its address.
    async fn serve(router: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    /// Write `request` (head, and a body if the caller supplied one), then read the WHOLE response:
    /// loop until end-of-stream with a 3 s tail timeout and a 4096-byte cap. A single `recv` can
    /// stop inside the headers and read a correct `408` as a false failure. Returns (time to the
    /// FIRST byte, the full response text).
    async fn exchange(addr: std::net::SocketAddr, request: Vec<u8>) -> (Duration, String) {
        let started = Instant::now();
        let sock = TcpStream::connect(addr).await.expect("connect");
        let (mut rd, mut wr) = tokio::io::split(sock);
        // Detached: a write that outlives the server's interest in it (an over-limit body, say) must
        // not hang the test — dropping the read half at the end drops the connection under it.
        tokio::spawn(async move {
            let _ = wr.write_all(&request).await;
            let _ = wr.flush().await;
        });
        let mut buf = [0u8; 4096];
        let mut out: Vec<u8> = Vec::new();
        let mut first: Option<Duration> = None;
        loop {
            match tokio::time::timeout(Duration::from_secs(3), rd.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => {
                    if first.is_none() {
                        first = Some(started.elapsed());
                    }
                    out.extend_from_slice(&buf[..n]);
                    if out.len() > 4096 {
                        break;
                    }
                }
            }
        }
        (
            first.unwrap_or_else(|| started.elapsed()),
            String::from_utf8_lossy(&out).into_owned(),
        )
    }

    /// The head-only shape: the request head with a declared body that is never sent.
    async fn raw(addr: std::net::SocketAddr, head: &str) -> (Duration, String) {
        exchange(addr, head.as_bytes().to_vec()).await
    }

    /// A stalled body must end the hold: `408`, `Connection: close`, the bound named, promptly.
    #[tokio::test]
    async fn stalled_body_is_answered_408_within_the_deadline() {
        // 150 ms, not the configured 30 s: the assertion is about WHICH requests the deadline fires
        // on, not how long the operator set it to.
        let addr = serve(app(Duration::from_millis(150))).await;
        let (first_byte, text) = raw(
            addr,
            "POST /body HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: 100000\r\n\r\n",
        )
        .await;

        assert!(
            text.starts_with("HTTP/1.1 408"),
            "a body that never arrives must be answered, not parked: {}",
            text
        );
        assert!(
            text.to_ascii_lowercase().contains("connection: close"),
            "the declared body was never consumed, so the connection must not be reused: {}",
            text
        );
        assert!(
            text.contains("\"status\":408") && text.contains("\"body_deadline_seconds\""),
            "the body must name the bound: {}",
            text
        );
        assert!(
            first_byte < Duration::from_secs(2),
            "the deadline must fire promptly, took {first_byte:?}"
        );
    }

    /// The bytes the client sends are the bytes the handler sees.
    #[tokio::test]
    async fn complete_body_reaches_the_handler_unchanged() {
        let addr = serve(app(Duration::from_secs(30))).await;
        let payload = r#"{"program_slug":"x","contact":{"email":"a@b.test"}}"#;
        let (_, text) = raw(
            addr,
            &format!(
                "POST /body HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            ),
        )
        .await;

        assert!(text.starts_with("HTTP/1.1 200"), "{}", text);
        assert!(
            text.ends_with(payload),
            "the handler must see the raw request bytes, unmodified: {}",
            text
        );
    }

    /// A body over axum's own 2 MiB limit gets the extractor's own rejection: the deadline must not
    /// become a wider hole for a single unauthenticated request to pin memory.
    ///
    /// The bytes are actually sent, because axum's `Bytes` extractor does not police the declared
    /// `Content-Length` up front — it errors the moment the body it has collected exceeds the limit.
    #[tokio::test]
    async fn body_over_the_default_limit_is_rejected_413() {
        let addr = serve(app(Duration::from_secs(30))).await;
        let oversized = vec![b'x'; 2 * 1024 * 1024 + 1];
        let head = format!(
            "POST /body HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            oversized.len()
        );
        let mut request = head.into_bytes();
        request.extend_from_slice(&oversized);
        let (_, text) = exchange(addr, request).await;
        assert!(
            text.starts_with("HTTP/1.1 413"),
            "an over-limit body must keep the extractor's own 413: {}",
            text
        );
    }

    /// A request that declares NO body is passed through untouched: with no declared body there is
    /// nothing that can be waited for, so the layer must not introduce a wait of its own. This is the
    /// scope leg — it is what lets this app's flat router carry the layer without the contract of its
    /// GET-only routes changing, and it is measured live in both phases.
    #[tokio::test]
    async fn request_without_a_declared_body_is_not_bounded() {
        let addr = serve(app(Duration::from_millis(150))).await;
        // A GET that declares a body it will never send: exactly the shape that would park a
        // body-reading route, on a route that reads no body at all.
        let (first_byte, text) = raw(
            addr,
            "GET /nobody HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\n",
        )
        .await;
        assert!(text.starts_with("HTTP/1.1 200"), "{}", text);
        assert!(
            first_byte < Duration::from_millis(100),
            "a request that declares no body must not be waited for, took {first_byte:?}"
        );

        // …and neither is a request that declares an EMPTY body.
        let addr = serve(app(Duration::from_millis(150))).await;
        let (first_byte, text) = raw(
            addr,
            "POST /body HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(!text.starts_with("HTTP/1.1 408"), "{}", text);
        assert!(
            first_byte < Duration::from_millis(100),
            "a request that declares an empty body must not be waited for, took {first_byte:?}"
        );
    }

    /// The ordering leg, and the reason this module does not pre-read the body: a handler whose
    /// FIRST extractor is a credential check must answer `401` at once, even though the request
    /// declares a body that never arrives. Before this module the app's accidental whole-request
    /// budget let this through too (the handler ran and rejected before 30 s), so this is a contract
    /// leg that must NOT move — and a body-pre-reading layer would have moved it to a `408` at the
    /// bound on every one of this app's 70 credential-then-body handlers.
    #[tokio::test]
    async fn handler_that_answers_without_reading_the_body_is_not_waited_for() {
        // A 2 s deadline: if the layer waited for the body, this test would take 2 s and answer 408.
        let addr = serve(app(Duration::from_secs(2))).await;
        let (first_byte, text) = raw(
            addr,
            "POST /authorized HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: 100000\r\n\r\n",
        )
        .await;
        assert!(text.starts_with("HTTP/1.1 401"), "{}", text);
        assert!(
            first_byte < Duration::from_millis(300),
            "an unauthenticated request must not wait for its body, took {first_byte:?}"
        );
    }

    /// The other half of the same argument: once the body HAS arrived, the handler's own work is not
    /// bounded. A 150 ms deadline with a handler that sleeps 400 ms AFTER reading its body must
    /// answer `200`, not `408` — before this module the app's `TimeoutLayer` answered `408` for any
    /// request still in flight at the deadline, which is exactly how in-progress work was dropped.
    #[tokio::test]
    async fn handler_is_not_cut_off_once_its_body_has_arrived() {
        let addr = serve(app(Duration::from_millis(150))).await;
        let payload = "{}";
        let (first_byte, text) = raw(
            addr,
            &format!(
                "POST /slow HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            ),
        )
        .await;
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "work in progress past the body deadline must not be cut off: {}",
            text
        );
        assert!(
            first_byte >= Duration::from_millis(350),
            "the handler's own 400 ms must actually have run, took {first_byte:?}"
        );
    }
}
