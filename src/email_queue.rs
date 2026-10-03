//! Pending email queue — scheduled/delayed email sends (follow-ups, reminders).
//!
//! `schedule_email` inserts a row into `pending_emails`; a background ticker
//! (`process_due_emails` loop) flushes due rows via the tenant-aware SMTP sender.
//!
//! `status` vocabulary (kanban t_9d711589, extended by t_44a990da):
//!   `pending`  — queued and due to be flushed by the ticker. The ONLY status the ticker reads.
//!                `attempts = 0` means "never tried"; `attempts > 0` means a prior attempt FAILED
//!                and the row has been RE-ARMED by pushing `send_at` forward — `send_at` is then
//!                the next attempt time, not the original one. See the POLICY block below.
//!   `sent`     — delivered (`sent_at` set).
//!   `failed`   — the send raised and this row has used up `MAX_SEND_ATTEMPTS` attempts.
//!                **Terminal**: the ticker never reads a `failed` row, so this is a DEAD LETTER.
//!                Recorded with `attempts` + `last_error`, logged at ERROR, and listed on
//!                `GET /api/v1/admin/email-queue` so a human can see it.
//!   `retired`  — deliberately withdrawn (never sent, never will be); the reason lives in
//!                `last_error`. Used to clear probe/fixture residue out of the dead-letter set.
//!   `cancelled`— reserved; no writer in this tree.
//!
//! A failure used to be visible ONLY in `last_error` — no log line, no panel — and 18 dead rows sat
//! unnoticed for 12 days (measured 2026-10-02, kanban t_9d711589). Hence the ERROR line below and
//! the admin read route that lists the dead letters.
//!
//! A timed-out tenant dial is NOT a separate state (kanban t_05b6efa2): `sender::deliver_via`
//! bounds ONE dial at `sender::TENANT_SMTP_DEADLINE` (10 s), so a tenant mail server that accepts
//! the connection and stays silent now fails in 10 s with `last_error` naming the bound instead of
//! parking this loop. The flush reads at most LIMIT 100 due rows and runs up to [`FLUSH_CONCURRENCY`]
//! ACCOUNT batches at a time (kanban t_ed6c2124): what ONE unreachable tenant server is allowed to
//! cost the OTHER tenants' mail is the FAIRNESS block below, and how many such servers can cost the
//! tick a bound at once is the CONCURRENCY block below.
//!
//! # FAIRNESS OF THE FLUSH (kanban t_96695538, widened by t_ed6c2124)
//!
//! DECISION: **arm B — an ACCOUNT-scoped failure costs this tick ONE bound, not one bound per queued
//! row.** The change is that the first account-scoped failure of a tick ENDS that account's turn for
//! this tick, so every LATER due row for that same account is skipped for the rest of the tick — no
//! dial, no write. One unreachable tenant server can therefore delay the other tenants' mail by at
//! most one `TENANT_SMTP_DEADLINE` per tick, not by one per queued row. (kanban
//! t_ed6c2124 kept this rule and re-expressed it as the ACCOUNT BATCH — one account's due rows are
//! one unit of work, flushed one dial at a time, and `sent` keeps its exact meaning. See the
//! CONCURRENCY block below.)
//!
//! MEASURED (this card's own probe: 3 due rows for one account whose mail server is a silent sink,
//! plus a 4th row for ANOTHER account): BEFORE, the tick dialled the sink 3 times (3 x 10 s) and the
//! other account's row only settled at the end of that ~30 s; AFTER, 1 dial, the other account's row
//! settled at ~10 s, and the two skipped rows were still `pending` with `attempts = 0` and no
//! `last_error` — they were never attempted.
//!
//! ARMS REJECTED:
//!   * arm A taken with the ROW as its unit (`buffer_unordered(4)` over the due rows) — a wider flush
//!     of that shape DIVIDES the delay, it does not bound it: with the query's own worst case (100 due
//!     rows for one unreachable tenant) a 4-wide flush still spends 25 x 10 s parked on that tenant,
//!     so the other tenants' mail is still late. It would also open up to 4 simultaneous SMTP dials to
//!     ONE tenant's mail server (a server whose credentials this app holds, and whose operator sees
//!     the connection burst) and interleave the per-row `sent` counter / DB writes. kanban t_ed6c2124
//!     later took arm A up with the ACCOUNT as the unit instead, which answers exactly that objection
//!     — a batch is flushed one dial at a time, so no server is ever dialled twice at once; see the
//!     CONCURRENCY block below for the measurement that decided it.
//!   * arm C (status quo + a log line) — the delay IS the defect (up to ~17 min of other tenants' mail
//!     per tick at LIMIT 100), so it cannot be the fix. Its useful half is KEPT: a flush that failed,
//!     deferred something, or spent >= 1 s logs ONE INFO line naming how long the tick took, so a
//!     slow tick can be told from a stuck one.
//!
//! WHICH failures are account-scoped ([`failure_scope`]):
//!   * ROW-scoped — the account's other rows can still go out, so they are NOT deferred: a bad
//!     `template_type` ([`sender::NO_TEMPLATE_PREFIX`]), a recipient that provably cannot receive
//!     mail ([`crate::security::email_addr::RECIPIENT_REFUSED`], kanban t_f56f4a79), an unparseable
//!     recipient ([`sender::INVALID_RECIPIENT_PREFIX`]). None of these predicts the NEXT row.
//!   * ACCOUNT-scoped — a property of the account's transport/config, so its next due row fails the
//!     same way this tick: the bound (`... did not answer within 10s ...`), any other
//!     `SMTP send failed: ...`, a host the SSRF gate refuses, `Invalid SMTP host:`,
//!     `Invalid from address:`, and the platform provider's own failure when the account rides
//!     platform mail. Anything NOT named ROW-scoped above is account-scoped, so an unrecognised
//!     failure text defaults to "defer the account" rather than to "burn a bound per row".
//!
//! WHAT HAPPENS TO A DEFERRED ROW: **nothing is written for it.** It stays `status = 'pending'` with
//! its `attempts` and `last_error` EXACTLY as they were, because an attempt that never happened must
//! not be recorded as one (that would consume the bounded-retry budget of t_44a990da and lie about
//! it). It is simply still due, so the NEXT tick (30 s) picks it up — while one account's server is
//! down its rows march at one bound per tick, and every other account's mail is not delayed behind
//! them. `failed` is still TERMINAL and the retry policy is untouched.
//!
//! # CONCURRENCY OF THE FLUSH (kanban t_ed6c2124)
//!
//! DECISION: **arm A, taken with the ACCOUNT as the unit of concurrency** — up to [`FLUSH_CONCURRENCY`]
//! tenant mail servers are dialled at the same time. One account's due rows are never split across
//! those slots (they are one BATCH, flushed one dial at a time), so the fairness rule above is
//! unchanged and no mail server is ever dialled twice at once.
//!
//! THE RESIDUAL THIS CLOSES: after t_96695538 one dead server costs the tick ONE bound, but the flush
//! was still SEQUENTIAL, so K DISTINCT unreachable senders cost one tick K x `TENANT_SMTP_DEADLINE`
//! (up to ~17 min at the query's LIMIT 100), and a large HEALTHY due set was still flushed one dial at
//! a time, delaying every other account's mail behind the sum of those dials.
//!
//! MEASURED (this card's own probe — `/opt/swift/audits/t_ed6c2124/probe.py`, run against the deployed
//! binary on both sides of the change; 6 due rows for 4 DISTINCT accounts whose mail server is a
//! silent sink, plus 1 due row for a 5th account that fails before any transport). BEFORE
//! (8a22cfae1c51958d): 4 dials, SERIAL — 30.2 s between the first and the 4th connection, tick
//! `flush took 40.2s`, and the 5th account's row was attempted only 40.4 s after the first dial.
//! AFTER: the same 4 dials, all four connections accepted within a fraction of a second of each
//! other, tick `flush took ~10 s` — exactly ONE bound — and the 5th account's row is attempted
//! immediately instead of behind the dead senders. The dial count stays 4 (not 6: the t_96695538 skip
//! rule is preserved, D1's 2nd/3rd rows are still deferred with `attempts = 0`), and the two
//! accounts' `sent`/`failed`/`deferred`/`stalled_accounts` figures are unchanged.
//!
//! WHY 8: the query's ceiling is LIMIT 100 rows and the ticker's cadence is 30 s, so that window can
//! only be drained inside one cadence at >= 3.3 dials/s — 8 slots carry a 1-2 s healthy dial
//! (DNS + TCP + STARTTLS + dialogue). It sits well inside the DB pool (`DB_MAX_CONNECTIONS` default
//! 20; at most one short query per in-flight batch), and the 8 dials go to 8 DIFFERENT servers by
//! construction, so no provider sees a burst.
//!
//! WHAT THIS DOES NOT DO (stated, not implied away): the bound is still per WAVE, so K distinct dead
//! senders cost `ceil(K / FLUSH_CONCURRENCY)` x `TENANT_SMTP_DEADLINE` of one tick — 13 waves ≈ 130 s
//! at the query's worst case, against 1000 s sequentially. That is the residual divided by a measured
//! factor, not abolished; the two arms that would have bounded it outright were rejected below.
//!
//! PER-ROW DB WRITES: unchanged. The same one `UPDATE` per row, from whichever batch slot finished
//! that row; at most [`FLUSH_CONCURRENCY`] of them are in flight at once.
//!
//! THE TICK'S LOG LINE: the same line and the same fields (`due`, `sent`, `failed`, `deferred`,
//! `stalled_accounts`, `elapsed_ms`), plus `accounts` (how many account batches the tick ran, i.e. the
//! most dials it could have made) and `concurrency`. `elapsed_ms` is the tick's own wall clock, so
//! this change reads as 40.3 s -> ~10 s on the same measurement.
//!
//! ARMS REJECTED:
//!   * arm B (a wall-clock budget: stop the flush once the tick has spent X seconds, leave the
//!     untouched rows `pending`) — DERIVED FROM THE CODE AND THE PRE PROBE ABOVE, not built: the flush
//!     reads `ORDER BY send_at ASC`, so the rows a truncated tick leaves behind are the NEWEST due
//!     rows, and each later tick starts again at the OLDEST still-due row — the window advances only
//!     as fast as the budget allows. At the measured cost (one dead sender = 10 s of any tick) and
//!     X = 20 s a tick dials exactly 2 dead senders and stops, so 100 dead accounts at the head of the
//!     window delay every row behind them by ~50 ticks (~25 min) — WORSE than the 40.2 s tick it
//!     removes, and it lowers healthy throughput instead of raising it (any healthy dial slower than
//!     X/rows truncates the window every tick). X would also have to stay below the 30 s cadence to
//!     mean anything, which is what makes the truncation so aggressive. Concurrency pays the same
//!     bound once and still finishes the whole window.
//!   * arm C (do nothing, with numbers) — rejected on the numbers: this fleet's queue is empty TODAY
//!     (measured 2026-10-02: `pending_emails` = `retired=18` only, 0 pending rows), but the ticker is
//!     this app's only outbound path and the residual is a property of the code, not of today's
//!     traffic. Its "make the residual visible" half is already shipped anyway (the tick's own line
//!     names `stalled_accounts` and `elapsed_ms`), so C would have left the card's own acceptance
//!     probe with nothing to measure.
//!
//! `GET /api/v1/admin/email-queue` needs NO change for this arm: a deferred row is indistinguishable
//! from a row that is queued-but-not-yet-due (`pending`, `attempts = 0`), which is exactly the truth
//! — it was not attempted. The thing that names the stall is the tick's own INFO line
//! (`email ticker flush took ...`), not the row.
//!
//! # POLICY (kanban t_44a990da): a failed send is retried a BOUNDED number of times
//!
//! DECISION: **arm A — bounded retry.** Arm B ("keep it terminal") was rejected because the errors
//! this arm actually receives are *transient by construction*: the queue's own transport report is
//! at worst `SMTP send failed: <host:port> did not answer within 10s` / a connection reset /
//! `Mailgun returned 5xx` — a mail server restarting, a provider blip, a TCP timeout. SMTP itself
//! answers a temporary refusal with a **4xx** by definition (RFC 5321 §4.2.1: "transient negative
//! completion reply" — the verb is *try again later*), and nothing else in this app retries an
//! outbound mail: the ticker is the last hop. Making the FIRST error final therefore means silently
//! losing that recipient's email, which is the defect this card exists to close.
//!
//! SHAPE (chosen for the smallest honest change):
//!   * `MAX_SEND_ATTEMPTS = 3` **total** sends (1 initial + 2 retries) — the same budget the app's
//!     own webhook sender uses (`delivery/webhook.rs`, "retry (3 attempts, exponential backoff)").
//!   * the retry is DEFERRED, not a `sleep` inside this loop: the failure arm pushes `send_at`
//!     forward by `RETRY_BACKOFF` (5 min, then 30 min — a mail server that is restarting needs
//!     minutes, and this loop must not park, see t_05b6efa2). Because the flush predicate is
//!     `status = 'pending' AND send_at <= NOW()`, re-arming `send_at` *is* the backoff: no new
//!     column, no new status, no migration, and `idx_pending_emails_due` still covers the row.
//!   * the row is written `failed` — TERMINAL — only when the 3rd attempt also raises, so the
//!     dead-letter set stays honest and small.
//!
//! DEDUPE IS UNCHANGED. `lifecycle_emails::already_emailed` counts `pending_emails` rows for
//! (account, to_email, template_type) created in the last 24 h **regardless of status**. A retry
//! UPDATEs the SAME row: no row is inserted, `created_at` is not touched, so the count and this
//! function's answer are identical before and after a retry. (Proven: the retried row keeps its id
//! and `created_at` — see the card's audit.)
//!
//! NOT resurrected (and deliberately not a goal): rows already sitting `failed` from before this
//! change stay terminal, and `failed` is NOT added to the ticker's predicate. The live table holds
//! 0 such rows (measured 2026-10-02), so nothing is stranded by that choice.
//!
//! # An undeliverable recipient (kanban t_f56f4a79)
//!
//! A row whose recipient provably cannot receive mail is NOT special-cased in this loop. The
//! refusal happens at the send seam (`security::email_addr::refuse_undeliverable_recipient`, called
//! by both `email_provider::deliver` and `sender::deliver_via`), so this loop sees an ordinary
//! `Err` and treats it like any other failure: the reason (`recipient-refused: …`) is recorded in
//! `last_error`, the row is re-armed once, and it settles as a terminal dead letter on the 3rd
//! attempt. Every one of those attempts makes NO provider request and opens NO socket — the guard
//! runs before the transport — so the cost is a bounded log line, not a send and a bounce. The
//! retry budget itself is unchanged (see POLICY above); a permanently-undeliverable row is instead
//! made visible to the operator, which is what the dead-letter surface exists for.

use crate::delivery::sender;
use crate::state::AppState;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

/// How many send attempts a queued row may receive before it becomes a TERMINAL dead letter
/// (`status = 'failed'`). Three TOTAL sends — one initial plus two retries — the same budget the
/// app's own webhook sender uses (`delivery/webhook.rs`). See the module's POLICY block.
pub const MAX_SEND_ATTEMPTS: i32 = 3;

/// How long to wait before the NEXT attempt, indexed by the number of failures already recorded:
/// `RETRY_BACKOFF[0]` is the wait after the 1st failure, `RETRY_BACKOFF[1]` after the 2nd.
///
/// 5 min then 30 min, not seconds: the failure this exists for is a mail server being restarted or
/// a provider blip, and a retry that lands back in the same second is the same failure again. The
/// wait is applied by moving `send_at` forward, so the ticker (30 s) simply skips the row until
/// then — it never sleeps on it (see t_05b6efa2).
pub const RETRY_BACKOFF: [Duration; (MAX_SEND_ATTEMPTS - 1) as usize] =
    [Duration::from_secs(5 * 60), Duration::from_secs(30 * 60)];

/// What to do with a row whose `failures`-th send attempt just raised (kanban t_44a990da).
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Not the last attempt: re-arm the row and try again after the backoff.
    Retry(Duration),
    /// `MAX_SEND_ATTEMPTS` used up: write `failed` and stop. The row is now a dead letter.
    DeadLetter,
}

/// THE retry rule — pure, so it can be unit-tested against the pre-change behaviour where the
/// first failure was final. `failures` is the count INCLUDING the attempt that just failed.
pub fn disposition_after_failure(failures: i32) -> Disposition {
    if failures < MAX_SEND_ATTEMPTS {
        // `failures` is >= 1 on every call; clamp so a malformed row cannot panic the ticker.
        let idx = (failures.max(1) as usize - 1).min(RETRY_BACKOFF.len() - 1);
        Disposition::Retry(RETRY_BACKOFF[idx])
    } else {
        Disposition::DeadLetter
    }
}

/// Whether a send failure is a property of the ROW alone or of the ACCOUNT's transport, which
/// decides the fairness rule of this tick (kanban t_96695538) — see the module's FAIRNESS block.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FailureScope {
    /// Only this row's own input is bad; the account's other rows can still go out.
    Row,
    /// The account's transport/config failed, so its next due row fails the same way this tick.
    Account,
}

/// THE classification the tick's fairness rule rests on — pure, so it is unit-tested.
///
/// ROW-scoped is a CLOSED list on purpose (three producers, all named as consts so they cannot
/// drift); everything else — including a text no version of this app produces today — is
/// account-scoped. Deferring a row costs one 30 s tick; dialling an account whose transport has just
/// failed costs the WHOLE tick another bound, which is the defect this card removes.
pub fn failure_scope(err: &str) -> FailureScope {
    const ROW_SCOPED: [&str; 3] = [
        sender::NO_TEMPLATE_PREFIX,
        crate::security::email_addr::RECIPIENT_REFUSED,
        sender::INVALID_RECIPIENT_PREFIX,
    ];
    if ROW_SCOPED.iter().any(|p| err.starts_with(p)) {
        FailureScope::Row
    } else {
        FailureScope::Account
    }
}

/// Is this failure the ACCOUNT's (so its remaining due rows are deferred this tick), or the ROW's?
/// ONE place, so the tick's three uses of the rule — the defer count, the `stalled` flag and the
/// break out of a batch — can never disagree.
fn defers_account(err: &str) -> bool {
    failure_scope(err) == FailureScope::Account
}

/// THE fairness rule, expressed for a batch (kanban t_96695538, re-expressed by t_ed6c2124): after
/// the failure of the row at `index` (0-based) of a batch of `len` rows, how many of that account's
/// rows this tick DEFERS — no dial, no write — for the next tick. Pure, so it is unit-tested.
///
/// A ROW-scoped failure predicts nothing about the account's next row, so it defers nothing and the
/// batch carries on; an ACCOUNT-scoped failure leaves exactly the rows behind it, which would fail
/// the same way this tick.
pub fn deferred_after_failure(len: usize, index: usize, err: &str) -> usize {
    if defers_account(err) {
        len.saturating_sub(index + 1)
    } else {
        0
    }
}

/// How many tenant mail servers ONE tick may be dialling at the same time (kanban t_ed6c2124).
///
/// The unit of concurrency is the ACCOUNT, never the row: [`group_due_by_account`] turns the tick's
/// due rows into one batch per account and a batch is flushed one dial at a time, so this number is
/// also the most simultaneous SMTP dials the tick can make — and no mail server is ever dialled twice
/// at once. See the module's CONCURRENCY block for why 8 and for what it does NOT bound.
pub const FLUSH_CONCURRENCY: usize = 8;

/// One due row exactly as the flush query returns it: `id, account_id, to_email, template_type,
/// vars, attempts`.
pub type DueRow = (Uuid, Uuid, String, String, Value, i32);

/// One account's due rows for this tick, in the query's order (`send_at` ASC) — the UNIT OF WORK of
/// the flush (kanban t_ed6c2124).
#[derive(Debug)]
pub struct AccountBatch {
    pub account_id: Uuid,
    pub rows: Vec<DueRow>,
}

/// THE grouping the tick's concurrency rests on — pure, so it is unit-tested.
///
/// The query is `ORDER BY send_at ASC`, so batches come out in the order of each account's OLDEST due
/// row and the rows inside a batch keep that order. Grouping makes "one dial per account, at most" a
/// structural fact instead of a rule the loop has to remember: an account exists in exactly one batch,
/// so two slots can never dial the same server, and once the batch's dial fails account-scoped there
/// is nothing left of that account to dial but the rows of that same batch, which are deferred.
pub fn group_due_by_account(due: Vec<DueRow>) -> Vec<AccountBatch> {
    let mut batches: Vec<AccountBatch> = Vec::new();
    let mut seen: HashMap<Uuid, usize> = HashMap::new();
    for row in due {
        // `.copied()` ends the borrow of `seen` before either arm runs — the `entry`-API trap.
        match seen.get(&row.1).copied() {
            Some(i) => batches[i].rows.push(row),
            None => {
                seen.insert(row.1, batches.len());
                batches.push(AccountBatch {
                    account_id: row.1,
                    rows: vec![row],
                });
            }
        }
    }
    batches
}

/// What ONE account's batch did this tick — the per-account half of the tick's counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchOutcome {
    pub sent: usize,
    pub failed: usize,
    /// Rows left untouched for the next tick after this account's dial failed account-scoped.
    pub deferred: usize,
    /// The account's dial failed ACCOUNT-scoped (it is named on the tick's `stalled_accounts`).
    pub stalled: bool,
}

impl BatchOutcome {
    fn merge(&mut self, other: BatchOutcome) {
        self.sent += other.sent;
        self.failed += other.failed;
        self.deferred += other.deferred;
        self.stalled |= other.stalled;
    }
}

/// Record ONE failed attempt on ONE row: re-arm it (same row, same `created_at`, `send_at` pushed
/// past the backoff — so `already_emailed` is unaffected) or write the terminal dead letter on the
/// last attempt. THE log lines are the ones t_9d711589 / t_44a990da added; the recipient address is
/// deliberately NOT logged (this app's convention) — the id joins to the row
/// `GET /api/v1/admin/email-queue` shows.
async fn record_failure(
    state: &AppState,
    id: Uuid,
    account_id: Uuid,
    template_type: &str,
    attempts: i32,
    err: &str,
) {
    let failures = attempts + 1;
    match disposition_after_failure(failures) {
        Disposition::Retry(backoff) => {
            let next_attempt_at =
                chrono::Utc::now() + chrono::Duration::seconds(backoff.as_secs() as i64);
            tracing::warn!(
                email_id = %id,
                account_id = %account_id,
                template_type = %template_type,
                attempt = failures,
                of = MAX_SEND_ATTEMPTS,
                retry_in_secs = backoff.as_secs(),
                "queued email could not be sent — re-armed for a bounded retry: {err}"
            );
            let _ = sqlx::query(
                "UPDATE pending_emails SET attempts = $2, last_error = $3, send_at = $4 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(failures)
            .bind(err)
            .bind(next_attempt_at)
            .execute(&state.db)
            .await;
        }
        Disposition::DeadLetter => {
            tracing::error!(
                email_id = %id,
                account_id = %account_id,
                template_type = %template_type,
                attempts = failures,
                "queued email could not be sent — DEAD LETTER (status='failed' after {failures} \
                 attempts, the ticker does not retry a failed row); see \
                 GET /api/v1/admin/email-queue: {err}"
            );
            let _ = sqlx::query(
                "UPDATE pending_emails SET status = 'failed', attempts = $2, last_error = $3 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(failures)
            .bind(err)
            .execute(&state.db)
            .await;
        }
    }
}

/// Flush ONE account's due rows — the unit of work the tick runs at most [`FLUSH_CONCURRENCY`] of at
/// once (kanban t_ed6c2124), and the reason a tenant's mail server is never dialled twice
/// concurrently. One dial at a time, in order; stops at the batch's first ACCOUNT-scoped failure and
/// leaves that account's remaining rows untouched for the next tick (the t_96695538 rule).
pub async fn flush_account_batch(state: &AppState, batch: AccountBatch) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    let total = batch.rows.len();
    for (index, (id, _account_id, to, template_type, vars, attempts)) in
        batch.rows.into_iter().enumerate()
    {
        let result =
            sender::send_template_by_type(&state.db, batch.account_id, &to, &template_type, &vars)
                .await;
        match result {
            Ok(_) => {
                let _ = sqlx::query(
                    "UPDATE pending_emails SET status = 'sent', sent_at = NOW() WHERE id = $1",
                )
                .bind(id)
                .execute(&state.db)
                .await;
                outcome.sent += 1;
            }
            Err(e) => {
                outcome.failed += 1;
                // FAIRNESS (t_96695538): an ACCOUNT-scoped failure defers the account's LATER rows
                // (the account is named stalled even when it was its last row — the dial failed).
                let account_scoped = defers_account(&e);
                if account_scoped {
                    outcome.stalled = true;
                    outcome.deferred += total - index - 1;
                }
                record_failure(state, id, batch.account_id, &template_type, attempts, &e).await;
                if account_scoped {
                    break;
                }
            }
        }
    }
    outcome
}

/// Queue an email to be sent at `send_at`.
pub async fn schedule_email(
    pool: &PgPool,
    account_id: Uuid,
    to_email: &str,
    template_type: &str,
    vars: &Value,
    send_at: chrono::DateTime<chrono::Utc>,
) -> Result<Uuid, String> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pending_emails (id, account_id, to_email, template_type, vars, send_at, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'pending')",
    )
    .bind(id)
    .bind(account_id)
    .bind(to_email)
    .bind(template_type)
    .bind(vars)
    .bind(send_at)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to queue email: {e}"))?;
    Ok(id)
}

/// Flush all due pending emails. Called by the background ticker.
///
/// The predicate reads `status = 'pending'` ONLY, and is UNCHANGED by the retry policy: a row that
/// failed and was re-armed is still `pending` with a future `send_at`, so it comes back through
/// this same query when its backoff elapses. `attempts` rides along so the failure arm can decide
/// retry-vs-dead-letter without a second read.
pub async fn process_due_emails(state: &AppState) -> usize {
    let due: Vec<DueRow> = sqlx::query_as(
        "SELECT id, account_id, to_email, template_type, vars, attempts FROM pending_emails
         WHERE status = 'pending' AND send_at <= NOW()
         ORDER BY send_at ASC
         LIMIT 100",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "pending_emails fetch failed — skipping this run");
        Default::default()
    });

    let due_count = due.len();
    let started = std::time::Instant::now();

    // CONCURRENCY (kanban t_ed6c2124): the tick's unit of work is ONE ACCOUNT's due rows (see
    // `group_due_by_account`) and it runs up to FLUSH_CONCURRENCY batches at the same time. A permit
    // is taken BEFORE each spawn, so at most FLUSH_CONCURRENCY flush tasks exist and this loop is
    // itself the queue: the tick's wall clock is ceil(accounts / FLUSH_CONCURRENCY) waves, not the
    // sum of every dial — while an account's rows are still dialled one at a time, so a tenant mail
    // server is never dialled twice at once.
    let batches = group_due_by_account(due);
    let accounts = batches.len();
    let slots = Arc::new(Semaphore::new(FLUSH_CONCURRENCY));
    let mut tasks: JoinSet<BatchOutcome> = JoinSet::new();
    for batch in batches {
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
            // The semaphore is never closed, so this cannot happen; if it ever did, starting no more
            // work is the safe half — the remaining rows stay `pending` and due for the next tick.
            tracing::error!(
                "email ticker: flush slots unavailable — the rest of this tick's due rows are left \
                 for the next tick"
            );
            break;
        };
        let state = state.clone();
        tasks.spawn(async move {
            let _slot = slot;
            flush_account_batch(&state, batch).await
        });
    }

    let mut tick = BatchOutcome::default();
    let mut stalled_accounts = 0usize;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(batch_outcome) => {
                if batch_outcome.stalled {
                    stalled_accounts += 1;
                }
                tick.merge(batch_outcome);
            }
            // A task can only end this way by panicking. The failure writes happen per row, after
            // each dial, so an interrupted batch simply leaves the rest of its rows due.
            Err(e) => tracing::error!(
                error = %e,
                "email ticker: an account's flush task ended without a result — that account's rows \
                 are untouched and still due for the next tick"
            ),
        }
    }

    // ARM C's useful half (kanban t_96695538): the tick's DELAY is named, so an operator can tell a
    // slow tick (a tenant server eating bounds) from a stuck one. One INFO line, only when the flush
    // had something to say — a short, clean, quiet flush logs nothing.
    let elapsed = started.elapsed();
    if due_count > 0 && (tick.failed > 0 || tick.deferred > 0 || elapsed >= Duration::from_secs(1))
    {
        tracing::info!(
            due = due_count,
            accounts,
            concurrency = FLUSH_CONCURRENCY,
            sent = tick.sent,
            failed = tick.failed,
            deferred = tick.deferred,
            stalled_accounts,
            elapsed_ms = elapsed.as_millis() as u64,
            "email ticker flush took {:.1}s for {} due row(s): {} sent, {} failed, {} deferred \
             to the next tick after an account's dial failed",
            elapsed.as_secs_f64(),
            due_count,
            tick.sent,
            tick.failed,
            tick.deferred
        );
    }
    tick.sent
}

/// Background ticker — flush due emails every 30s.
pub fn spawn_email_ticker(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // first tick immediately
        interval.tick().await;
        loop {
            interval.tick().await;
            let sent = process_due_emails(&state).await;
            if sent > 0 {
                tracing::info!("Email ticker sent {sent} queued email(s)");
            }
        }
    });
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;

    /// THE defect this policy exists to close (kanban t_44a990da): before it, the failure arm
    /// wrote `failed` on the FIRST error and nothing ever read `failed` again, so one transient
    /// SMTP refusal lost the recipient's email for ever. A first failure must NOT be terminal.
    /// This test fails on the pre-change rule (where disposition was unconditionally DeadLetter).
    #[test]
    fn a_first_failure_is_retried_not_terminal() {
        match disposition_after_failure(1) {
            Disposition::Retry(backoff) => assert_eq!(
                backoff,
                Duration::from_secs(300),
                "the first retry is 5 min out"
            ),
            Disposition::DeadLetter => panic!(
                "a first send failure must be re-armed, not written off — that is the silent-loss \
                 defect (t_44a990da)"
            ),
        }
    }

    /// The retry is BOUNDED: the wait grows, and the last allowed attempt is terminal — otherwise
    /// a permanently bad row (no such template, refused host) would be re-tried for ever.
    #[test]
    fn retries_are_bounded_and_the_last_attempt_is_terminal() {
        assert_eq!(
            disposition_after_failure(2),
            Disposition::Retry(Duration::from_secs(1800)),
            "the second retry is 30 min out"
        );
        assert_eq!(
            disposition_after_failure(MAX_SEND_ATTEMPTS),
            Disposition::DeadLetter,
            "attempt {MAX_SEND_ATTEMPTS} is the LAST one — it must be terminal"
        );
        assert_eq!(
            disposition_after_failure(0),
            Disposition::Retry(Duration::from_secs(300))
        );
    }

    /// The vocabulary the console and `already_emailed` rely on: `MAX_SEND_ATTEMPTS` is exactly
    /// `RETRY_BACKOFF.len() + 1`, i.e. one initial send plus one backoff slot per retry.
    #[test]
    fn the_backoff_table_covers_every_retry() {
        assert_eq!(RETRY_BACKOFF.len() as i32, MAX_SEND_ATTEMPTS - 1);
        assert!(
            MAX_SEND_ATTEMPTS >= 2,
            "a one-attempt budget is the old defect"
        );
    }
}

#[cfg(test)]
mod tick_fairness_tests {
    use super::*;

    fn account(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    /// A due row as the flush query returns it, carrying only what these rules read.
    fn due_row(id: u128, account_id: u128) -> DueRow {
        (
            Uuid::from_u128(id),
            Uuid::from_u128(account_id),
            format!("probe-{id}@probe-target.com"),
            "calc_summary".to_string(),
            Value::Null,
            0,
        )
    }

    const TRANSPORT: &str =
        "SMTP send failed: 209.222.97.179:2525 did not answer within 10s — the dial was abandoned \
         at the bound (TCP connect, STARTTLS or SMTP dialogue)";

    /// THE grouping the tick's concurrency rests on (kanban t_ed6c2124): an account's due rows are ONE
    /// unit of work, so its mail server is dialled at most once at a time however many rows it has.
    /// This test goes RED the moment the flush is changed to one task per ROW (or to a row-level
    /// `buffer_unordered`): the account then appears in as many batches as it has rows and two slots
    /// can dial the same server at once — the cost arm A was rejected for in t_96695538.
    #[test]
    fn one_account_is_always_exactly_one_batch() {
        let batches = group_due_by_account(vec![
            due_row(1, 100),
            due_row(2, 200),
            due_row(3, 100),
            due_row(4, 100),
            due_row(5, 200),
        ]);
        assert_eq!(
            batches.len(),
            2,
            "5 due rows for 2 accounts must be 2 batches (one server dialled at a time), not 5"
        );
        assert_eq!(
            batches[0].account_id,
            account(100),
            "the account with the OLDEST due row goes first"
        );
        assert_eq!(
            batches[0].rows.len(),
            3,
            "an account's rows are never split across batch slots"
        );
        assert_eq!(batches[1].account_id, account(200));
        assert_eq!(batches[1].rows.len(), 2);
        // the query's own order (oldest `send_at` first) survives inside a batch
        assert_eq!(batches[0].rows[0].0, Uuid::from_u128(1));
        assert_eq!(batches[0].rows[1].0, Uuid::from_u128(3));
        assert_eq!(batches[0].rows[2].0, Uuid::from_u128(4));
    }

    /// One slot would be the old sequential flush; this card's whole point is that K accounts no
    /// longer cost K bounds, which needs more than one server in flight — and the slots are also
    /// concurrent DB users, so they stay well inside the pool.
    #[test]
    fn the_flush_runs_more_than_one_account_at_a_time() {
        assert!(
            FLUSH_CONCURRENCY >= 2,
            "a one-slot flush is exactly the t_96695538 behaviour"
        );
        assert!(
            FLUSH_CONCURRENCY <= 16,
            "more simultaneous dials than the pool (DB_MAX_CONNECTIONS default 20) can serve is not a bound"
        );
    }

    /// THE rule (kanban t_96695538, unchanged by t_ed6c2124): a failure that is a property of the ROW
    /// must NOT defer the batch's later rows — they can still go out, and deferring them would cost
    /// them a tick for nothing.
    #[test]
    fn a_row_scoped_failure_never_defers_the_batch() {
        let cases = [
            format!("{} 'welcome'", sender::NO_TEMPLATE_PREFIX),
            format!(
                "{} the address is reserved",
                crate::security::email_addr::RECIPIENT_REFUSED
            ),
            format!("{} invalid address", sender::INVALID_RECIPIENT_PREFIX),
        ];
        for err in cases {
            assert_eq!(failure_scope(&err), FailureScope::Row, "{err}");
            assert!(
                !defers_account(&err),
                "{err} must not defer the account's later rows"
            );
            assert_eq!(
                deferred_after_failure(3, 0, &err),
                0,
                "a row-scoped failure deferred rows that could still send: {err}"
            );
        }
    }

    /// The defect arm: a TRANSPORT failure — what a dead tenant server produces — defers THAT
    /// account's later rows and nothing else. Before t_96695538 each of the account's due rows paid
    /// its own 10 s bound.
    #[test]
    fn a_transport_failure_defers_this_batchs_later_rows() {
        let cases = [
            TRANSPORT,
            "SMTP send failed: connection refused",
            "SMTP host refused by security policy: private address",
            "Invalid SMTP host: no such name",
            "Invalid from address: bad from",
            "Mailgun returned 502",
        ];
        for err in cases {
            assert_eq!(failure_scope(&err), FailureScope::Account, "{err}");
            assert!(defers_account(&err), "{err}");
            assert_eq!(
                deferred_after_failure(3, 0, &err),
                2,
                "rows 2 and 3 of the batch wait for the next tick: {err}"
            );
            assert_eq!(
                deferred_after_failure(1, 0, &err),
                0,
                "a one-row batch defers nothing — though the account is still named stalled: {err}"
            );
            assert_eq!(
                deferred_after_failure(3, 2, &err),
                0,
                "the account's LAST row leaves nothing behind: {err}"
            );
        }
    }

    /// An unrecognised text is account-scoped ON PURPOSE: deferring a row costs a 30 s tick, while
    /// dialling an account whose transport has just failed costs the whole tick another bound.
    #[test]
    fn an_unrecognised_failure_defers_rather_than_burning_a_bound() {
        assert_eq!(
            failure_scope("some failure text a later seam invented"),
            FailureScope::Account
        );
        assert!(defers_account("some failure text a later seam invented"));
    }

    /// The counters the tick logs are per-BATCH sums, so a counter added to `BatchOutcome` must not be
    /// droppable by `merge` — that would silently under-report a tick.
    #[test]
    fn batch_outcomes_merge_every_counter() {
        let mut total = BatchOutcome::default();
        total.merge(BatchOutcome {
            sent: 2,
            failed: 1,
            deferred: 0,
            stalled: false,
        });
        total.merge(BatchOutcome {
            sent: 0,
            failed: 1,
            deferred: 3,
            stalled: true,
        });
        assert_eq!(
            total,
            BatchOutcome {
                sent: 2,
                failed: 2,
                deferred: 3,
                stalled: true
            }
        );
    }
}
