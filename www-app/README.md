# IncentiveSwift `www-app/` — tracked source of the tenant app surface

**Decision (2026-09-23, lane `default`, kanban t_494552c5): this directory is the tracked
source of truth for the pages served at `app.incentiveswift.com`.** The deployment target is
`/opt/swift/nginx/www-app/incentiveswift/` (real files, not a symlink), and the versioned
copy of what nginx serves lives in the fleet frontends repo
(`/opt/swift/nginx`, remote `Swiftsoftware143/frontends`). Publish with:

    /opt/swift/bin/publish-incentiveswift-frontend.sh www-app/<file> [more files...]
    /opt/swift/bin/publish-incentiveswift-frontend.sh --diff www-app/<file>   # report drift only

The script copies only the paths you name, keeps a `.bak-<stamp>` of what it replaced and
sha256-verifies every copy, because a blanket `cp www-app/* <root>/` is destructive here — see
the warning below.

## What is here, and what it must match

| file | live path | note |
|---|---|---|
| `index.html` | `/` | the 7,856-byte sign-in shell (the SPA boot reads `localStorage.ws_fs_token`) |
| `login.html` | `/login` | nginx `location = /login` does `try_files /login.html =404` — an untracked copy 404s every app login |
| `iqs.html` | `/iqs.html` | IQS funnel runner |
| `dashboard/index.html` | `/dashboard/` | tenant dashboard |
| `integrations.html` | `/integrations.html` | BYOK Integration Center (tenant key entry) |
| `loyalty-checkin.html` | `/loyalty-checkin/<slug>` | public QR check-in page (no account) |
| `play.html` | `/play/<slug>` | public campaign page (no account) |

The Dockerfile does **not** copy this directory — the image only carries the binary and
`migrations/`, and nginx serves these files from its own root. So a stale file here never
breaks production by itself, but it does mislead the next person (or lane) who publishes.

## ⚠️ The decoy, kept on purpose

`archive/index-legacy-admin-spa-440k.html` was at this directory's `index.html` until
2026-09-23: a 440,466-byte retired **admin** SPA at the path of the app's entry page. The live
app root is 7,856 bytes. Publishing that file to the app root would have replaced the app with
an admin console. It is archived (git history has it either way) rather than deleted so the
distinction stays visible. It is also **broken**: its inline script does not parse
(`ui-js-check.py`: syntax error at its line 1845, an unterminated string in a `+'</td>'`
concat), so it is not a file to restore — the admin surface served today is
`www-admin/index.html` (135,151 B) at `admin.incentiveswift.com`, and no file of that 440 KB
hash is served anywhere under `/opt/swift/nginx`.

## The marketing root (`www/`) is a different surface — decided 2026-09-23

`www/index.html` is served at `incentiveswift.com/`; this directory's `index.html` is the app
entry at `app.incentiveswift.com/`. They were a two-variant pair: repo `9ada94d6f022` (200L)
carried a published `#pricing` section (Starter/Pro/Enterprise, `✓ Webhooks & Zapier`,
`✓ White-label`) plus a longer JSON-LD block; live `460cdd01f934` (299L) carried the cookie
banner, the `/terms.html` `/privacy.html` `/refunds.html` footer links and `openRegister()`
CTAs, and deliberately has **no pricing page** (its copy says "Upgrades and pricing are
managed inside your account"). Neither was a superset of the other, so this was a content
decision, not a copy: **the live variant is authoritative** — it is the newer product intent
(self-serve signup modal, legal pages, consent banner) and the retired pricing block
contradicted it — so `www/index.html` now holds the live variant byte-for-byte, the pricing
lists are intentionally gone, and no page links `#pricing` any more. Marketing files publish
through the fleet gate (`/opt/swift/fleet/marketing-www-parity.py`), never by hand-`cp`.

## Keeping it honest

`/opt/swift/scripts/spa-drift-check.py` pairs this directory with the nginx root
(`IncentiveSwift-app`), so drift shows up as a diff instead of living in someone's memory.
Run it after any edit here.
