# Email compliance — how to port it here (first written 2026-11-20)

CoreSwift now has opt-out, suppression, delivery evidence and per-campaign opt-outs, proved live. The
other five apps have none of it, and every one of them sends mail. This is the order that works, based on
what this app's code actually looks like — not on how CoreSwift looks.

## What is already true in this repo

* **One leaf spends a provider send:** `email_provider::deliver(pool, cfg, to, subject, text, html)`.
  Every path reaches it — the queue ticker, lifecycle stages, `delivery/sender.rs`, the Settings test
  send, `email::send_email_request`. Gating that one function covers all mail in this app.
* **The recipient guards already live there** (`refuse_undeliverable_recipient`, the harness-domain
  refusal). The compliance gate belongs beside them, and for the same reason: this is the last code
  before money is spent on a send.
* **The request context now carries the account.** `AuthUser` gained `account_id` (2026-11-20), resolved
  by the same call that resolves `is_admin`. Before that, no handler could key per-account mail state —
  suppression would have had to be global, which is wrong: one account's opt-out must never silence
  another account's mail.
* **There is no route allowlist in this app.** A public unsubscribe route therefore needs no allowlist
  entry, but it does need to be mounted deliberately (see step 3) rather than inheriting whatever the
  default layer does.
* **Migrations use a `zz_` prefix here**, not numbered files. The next one is free-form; name it
  `zz_is10_email_compliance.sql` and keep it additive.

## What to copy from CoreSwift, and what not to

Copy: `src/mail_compliance/` (token signing, the gate, the footer, the one-click headers, the public
page, the suppression list, the CSVs). It is self-contained except for two things noted below.

Do NOT copy: the Domains three-lane screen or the campaign runner. Those are CoreSwift product surfaces
with their own tables here; port them later, deliberately.

Adapt on the way in:
1. `AppState` and `Claims` — this app has `AuthUser { is_admin, account_id }`, not a `Claims` extension.
   The authenticated handlers take `AuthUser` and use `account_id`.
2. `tenant_id` — this app's tables hang off `tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)`.
   Key every compliance row by that tenant, resolved once per request.
3. The footer's postal address and the account name come from this app's own branding row, never a
   literal.
4. The provider credential: read it the way `email_provider::resolve` reads it. Do not add a second
   credential path — the deploy gate forbids reading third-party keys from the environment, and one
   resolver means the send path and the verification path cannot disagree.

## Order of work

1. **Migration**: `email_suppressions`, `email_message_log`, `email_events`, `email_consent` (same shapes
   as CoreSwift's 181). Additive only. Dry-run in a transaction, then apply.
2. **The gate in `email_provider::deliver`**: suppression check → allowance → footer and one-click
   headers → log the attempt. Add a `mail_class` parameter and make the CALLERS name it: lifecycle and
   campaign mail = bulk, notifications = notification, receipts and resets = transactional. Transactional
   is never stoppable and never capped.
3. **The public opt-out route**, mounted deliberately: `GET` renders the page (a complete document),
   `POST` handles both the browser form and the provider's one-click post. The signed token is the whole
   credential, and it names one account and one address.
4. **The console surface**: the account's delivery/bounce/complaint numbers, its do-not-mail list, the two
   CSVs. Without this, a shipped compliance layer is invisible to the customer who needs it.
5. **Provider reports**: a signed webhook per sending domain, attributing delivered/bounced/complained to
   the account that sent. CoreSwift learned that Mailgun's signing key is not readable through its API —
   the URL-token arm is what makes this work, so port BOTH arms.
6. **Guides**: this app's customer guide and admin guide. Fleet rule: guides change with the feature.

## Proof required before saying it works

Same standard CoreSwift was held to — live, on the running container, not read off the code:

* an unsigned/mis-signed provider report is refused and writes nothing;
* an address that opted out is refused at the leaf, with the refusal recorded as its own status;
* a hard bounce or complaint from the provider suppresses that address for that account only;
* an unsubscribe link from a real message opens the page, and one click writes exactly one suppression;
* the numbers the console shows move when an event arrives;
* a tenancy probe: one account's suppression is invisible to another.

## Two facts this fleet already learned, so do not rediscover them

1. An anonymous route filed on a credential list answers 401 while reading as open. Pin the class with a
   test (`/public/` and `/unsubscribe` may never sit on a key/internal list).
2. The customer console here may have no repo-to-served publisher (CoreSwift's had none). Check before
   committing a console change, or the change stays invisible.
