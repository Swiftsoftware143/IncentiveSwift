#!/usr/bin/env bash
# publish-mail.sh — install THIS app's mail client (repo www-mail/<slug>/) into its nginx root.
#
# WHY THIS EXISTS (kanban t_076c785d)
#   The private inbox's mail client is served from its own host (inbox.<domain>, root
#   /opt/swift/nginx/www-mail/<slug>). Until now that client existed ONLY as bytes on the host: no
#   repo copy, no publisher, nothing committed — so every fleet rule that assumes "the repo is the
#   authoring source of truth" was silently false for the whole mail front door, and a rebuild or a
#   restored host would have lost it.
#
# WHAT IT DOES
#   Compare each file in www-mail/<slug>/ with its served counterpart and copy the ones that differ,
#   sha256-verifying each write. A pair that already matches is a no-op (no write, no backup churn).
#
# THE FRESHNESS GUARD (same rule as the app's front-end publisher)
#   If a served copy differs AND is NEWER than the repo copy, this REFUSES (rc 2) and names both
#   sides: a served copy newer than its source is an unported edit — reconcile served -> repo,
#   commit, then publish. Without it this script would delete live work.
#
# USAGE
#   scripts/publish-mail.sh --diff     report only, writes NOTHING
#   scripts/publish-mail.sh            publish every differing file
#   scripts/publish-mail.sh mail.js    publish only the named paths
set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
APP="$(basename "$REPO")"
case "$APP" in
  FunnelSwift)        SLUG=funnelswift ;;
  WorkflowSwift)      SLUG=workflowswift ;;
  ADASwift)           SLUG=adaswift ;;
  missedcallrespondr) SLUG=missedcall ;;
  IncentiveSwift)     SLUG=incentiveswift ;;
  *) echo "publish-mail: unknown app dir '$APP'"; exit 2 ;;
esac

SRC="$REPO/www-mail/$SLUG"
DEST="/opt/swift/nginx/www-mail/$SLUG"
BACKUP="/opt/swift/backups/webroot-bak/www-mail/$SLUG"
STAMP="$(date '+%Y%m%d-%H%M%S')"
DIFF=0

[ -d "$SRC" ] || { echo "publish-mail: $SRC does not exist"; exit 2; }
[ -d "$DEST" ] || { echo "publish-mail: $DEST does not exist (nginx root missing)"; exit 2; }

args=()
for a in "$@"; do
  case "$a" in
    --diff) DIFF=1 ;;
    -*) echo "publish-mail: unknown flag $a"; exit 2 ;;
    *) args+=("$a") ;;
  esac
done

if [ "${#args[@]}" -gt 0 ]; then
  files=("${args[@]}")
else
  mapfile -t files < <(cd "$SRC" && find . -type f -printf '%P\n' | sort)
fi

rc=0
for rel in "${files[@]}"; do
  src="$SRC/$rel"; dst="$DEST/$rel"
  [ -f "$src" ] || { echo "REFUSED $rel — not in $SRC"; rc=2; continue; }
  if [ -f "$dst" ]; then
    s=$(sha256sum "$src" | cut -d' ' -f1); d=$(sha256sum "$dst" | cut -d' ' -f1)
    [ "$s" = "$d" ] && { echo "SAME    $rel"; continue; }
    if [ "$dst" -nt "$src" ]; then
      echo "REFUSED $rel — the SERVED copy is NEWER than the repo copy (unported edit)."
      echo "        reconcile served -> repo ($dst -> $src), commit, then publish."
      rc=2; continue
    fi
    [ "$DIFF" = 1 ] && { echo "DRIFT   $rel  repo=${s:0:16} live=${d:0:16}"; continue; }
    mkdir -p "$(dirname "$BACKUP/$rel")"
    cp -p "$dst" "$BACKUP/$rel.bak-$STAMP"
  else
    [ "$DIFF" = 1 ] && { echo "MISSING $rel  (live has no copy)"; continue; }
  fi
  mkdir -p "$(dirname "$dst")"
  cp -p "$src" "$dst"
  s=$(sha256sum "$src" | cut -d' ' -f1); d=$(sha256sum "$dst" | cut -d' ' -f1)
  if [ "$s" = "$d" ]; then
    echo "OK      $rel  ${s:0:16}"
  else
    echo "FAIL    $rel — copied bytes do not match the source"; rc=1
  fi
done
exit $rc
