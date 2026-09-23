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
distinction stays visible.

## Keeping it honest

`/opt/swift/scripts/spa-drift-check.py` pairs this directory with the nginx root
(`IncentiveSwift-app`), so drift shows up as a diff instead of living in someone's memory.
Run it after any edit here.
