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
//! parking this loop. The loop is still SEQUENTIAL (LIMIT 100); what one unreachable tenant server is
//! allowed to cost the OTHER tenants' mail is the FAIRNESS block below.
//!
//! # FAIRNESS OF THE FLUSH (kanban t_96695538)
//!
//! DECISION: **arm B — an ACCOUNT-scoped failure costs this tick ONE bound, not one bound per queued
//! row.** The flush stays sequential and `sent` keeps its exact meaning; the change is that the first
//! account-scoped failure of a tick puts that account on the tick's skip list, and every LATER due row
//! for that same account is skipped for the rest of this tick — no dial, no write. One unreachable
//! tenant server can therefore delay the other tenants' mail by at most one `TENANT_SMTP_DEADLINE`
//! per tick, not by one per queued row.
//!
//! MEASURED (this card's own probe: 3 due rows for one account whose mail server is a silent sink,
//! plus a 4th row for ANOTHER account): BEFORE, the tick dialled the sink 3 times (3 x 10 s) and the
//! other account's row only settled at the end of that ~30 s; AFTER, 1 dial, the other account's row
//! settled at ~10 s, and the two skipped rows were still `pending` with `attempts = 0` and no
//! `last_error` — they were never attempted.
//!
//! ARMS REJECTED:
//!   * arm A (`buffer_unordered(4)` and friends) — a wider flush DIVIDES the delay, it does not bound
//!     it: with the query's own worst case (100 due rows for one unreachable tenant) a 4-wide flush
//!     still spends 25 x 10 s parked on that tenant, so the other tenants' mail is still late. It
//!     would also open up to 4 simultaneous SMTP dials to ONE tenant's mail server (a server whose
//!     credentials this app holds, and whose operator sees the connection burst) and interleave the
//!     per-row `sent` counter / DB writes — cost the defect does not ask for.
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
use std::collections::HashSet;
use std::time::Duration;
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

/// THE classification the tick's skip list rests on — pure, so it is unit-tested.
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

/// This tick's skip list: the accounts whose dial already failed ACCOUNT-scoped, so their remaining
/// due rows are deferred instead of dialled (kanban t_96695538). Pure and cheap — the flush loop is
/// the only caller.
#[derive(Default)]
struct TickSkipList {
    deferred_accounts: HashSet<Uuid>,
}

impl TickSkipList {
    /// Leave this row for the next tick: a PREVIOUS row for the same account already failed
    /// account-scoped in this tick, so dialling it again would just pay another bound.
    fn should_skip(&self, account_id: Uuid) -> bool {
        self.deferred_accounts.contains(&account_id)
    }

    /// Record a failure for `account_id`. Returns true when this failure NEWLY puts the account on
    /// the skip list — an account-scoped failure for an account not already listed.
    fn note_failure(&mut self, account_id: Uuid, err: &str) -> bool {
        if failure_scope(err) == FailureScope::Account {
            self.deferred_accounts.insert(account_id)
        } else {
            false
        }
    }

    /// How many accounts had a dial fail account-scoped this tick (named in the tick's log line).
    fn accounts(&self) -> usize {
        self.deferred_accounts.len()
    }
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
    let due: Vec<(Uuid, Uuid, String, String, Value, i32)> = sqlx::query_as(
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
    let mut sent = 0;
    let mut failed_this_tick = 0usize;
    let mut deferred_this_tick = 0usize;
    let mut skips = TickSkipList::default();
    for (id, account_id, to, template_type, vars, attempts) in due {
        // FAIRNESS (kanban t_96695538): an account whose dial already failed ACCOUNT-scoped this tick
        // has its remaining due rows DEFERRED — no dial, no write, so the row keeps `attempts = 0`
        // and its `send_at`. See the module's FAIRNESS block.
        if skips.should_skip(account_id) {
            deferred_this_tick += 1;
            continue;
        }
        let result =
            sender::send_template_by_type(&state.db, account_id, &to, &template_type, &vars).await;

        match result {
            Ok(_) => {
                let _ = sqlx::query(
                    "UPDATE pending_emails SET status = 'sent', sent_at = NOW() WHERE id = $1",
                )
                .bind(id)
                .execute(&state.db)
                .await;
                sent += 1;
            }
            Err(e) => {
                failed_this_tick += 1;
                // FAIRNESS (kanban t_96695538): an ACCOUNT-scoped failure defers the rest of this
                // account's due rows for the remainder of this tick. A ROW-scoped failure (a bad
                // template_type, a refused recipient) predicts nothing about the next row and must
                // NOT defer it — see `failure_scope`.
                skips.note_failure(account_id, &e);
                // VISIBILITY (kanban t_9d711589). Before that line the row's ONLY record of the
                // failure was `last_error` — nothing logged, nothing listed — which is how 18 dead
                // letters sat unnoticed from 2026-09-20. The recipient address is deliberately NOT
                // logged (this app's convention); the id joins to the row an operator reads on
                // GET /api/v1/admin/email-queue, which does show it.
                //
                // POLICY (kanban t_44a990da): a first failure is no longer final. The row is
                // RE-ARMED (same row, same `created_at` — so `already_emailed` is unaffected) by
                // pushing `send_at` past the backoff, and only the LAST attempt writes the terminal
                // `failed` dead letter.
                let failures = attempts + 1;
                match disposition_after_failure(failures) {
                    Disposition::Retry(backoff) => {
                        let next_attempt_at = chrono::Utc::now()
                            + chrono::Duration::seconds(backoff.as_secs() as i64);
                        tracing::warn!(
                            email_id = %id,
                            account_id = %account_id,
                            template_type = %template_type,
                            attempt = failures,
                            of = MAX_SEND_ATTEMPTS,
                            retry_in_secs = backoff.as_secs(),
                            "queued email could not be sent — re-armed for a bounded retry: {e}"
                        );
                        let _ = sqlx::query(
                            "UPDATE pending_emails SET attempts = $2, last_error = $3, send_at = $4 \
                             WHERE id = $1",
                        )
                        .bind(id)
                        .bind(failures)
                        .bind(&e)
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
                            "queued email could not be sent — DEAD LETTER (status='failed' after \
                             {failures} attempts, the ticker does not retry a failed row); see \
                             GET /api/v1/admin/email-queue: {e}"
                        );
                        let _ = sqlx::query(
                            "UPDATE pending_emails SET status = 'failed', attempts = $2, \
                             last_error = $3 WHERE id = $1",
                        )
                        .bind(id)
                        .bind(failures)
                        .bind(&e)
                        .execute(&state.db)
                        .await;
                    }
                }
            }
        }
    }

    // ARM C's useful half (kanban t_96695538): the tick's DELAY is named, so an operator can tell a
    // slow tick (a tenant server eating bounds) from a stuck one. One INFO line, only when the flush
    // had something to say — a short, clean, quiet flush logs nothing.
    let elapsed = started.elapsed();
    if due_count > 0
        && (failed_this_tick > 0 || deferred_this_tick > 0 || elapsed >= Duration::from_secs(1))
    {
        tracing::info!(
            due = due_count,
            sent,
            failed = failed_this_tick,
            deferred = deferred_this_tick,
            stalled_accounts = skips.accounts(),
            elapsed_ms = elapsed.as_millis() as u64,
            "email ticker flush took {:.1}s for {} due row(s): {} sent, {} failed, {} deferred \
             to the next tick after an account's dial failed",
            elapsed.as_secs_f64(),
            due_count,
            sent,
            failed_this_tick,
            deferred_this_tick
        );
    }
    sent
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

    /// THE rule the skip list rests on (kanban t_96695538): a failure that is a property of the ROW
    /// must NOT defer the account's other rows — they can still go out, and deferring them would cost
    /// them a tick for nothing.
    #[test]
    fn a_row_scoped_failure_never_defers_the_account() {
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
            let mut skips = TickSkipList::default();
            assert!(
                !skips.note_failure(account(1), &err),
                "{err} must not put the account on the skip list"
            );
            assert!(
                !skips.should_skip(account(1)),
                "a row-scoped failure deferred a row that could still send: {err}"
            );
        }
    }

    /// The defect arm: a TRANSPORT failure — what a dead tenant server produces — defers THAT
    /// account only, and only once however many times it fails. Before this rule each of the
    /// account's due rows paid its own 10 s bound.
    #[test]
    fn a_transport_failure_defers_its_account_and_no_other() {
        let cases = [
            "SMTP send failed: 209.222.97.179:2525 did not answer within 10s — the dial was \
             abandoned at the bound (TCP connect, STARTTLS or SMTP dialogue)",
            "SMTP send failed: connection refused",
            "SMTP host refused by security policy: private address",
            "Invalid SMTP host: no such name",
            "Invalid from address: bad from",
            "Mailgun returned 502",
        ];
        for err in cases {
            assert_eq!(failure_scope(&err), FailureScope::Account, "{err}");
            let mut skips = TickSkipList::default();
            assert!(skips.note_failure(account(1), &err), "{err}");
            assert!(skips.should_skip(account(1)));
            assert!(
                !skips.should_skip(account(2)),
                "another account must never be deferred by this one's dead server: {err}"
            );
            assert_eq!(skips.accounts(), 1);
            // Idempotent: the same account failing repeatedly is still ONE stalled account.
            assert!(!skips.note_failure(account(1), &err));
            assert_eq!(skips.accounts(), 1);
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
    }
}
