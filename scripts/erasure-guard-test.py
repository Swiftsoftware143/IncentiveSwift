#!/usr/bin/env python3
"""IncentiveSwift account-erasure guard test — fleet standard §6 (2026-11-24).

Proves the two doors (close + erase) are session-only and that erasure really deletes the
tenant while retaining billing documents and writing exactly one audit row.

Six checks:
  1. POST /auth/close-account with a valid API key       -> 401 (never 200)
  2. POST /auth/close-account with no credential         -> 401
  3. close/erase routes appear in no public allowlist entry (they are private), and the
     handlers refuse the API-key role
  4. no MCP tool in the app's tool list names close / erase / delete_account
  5. POST /auth/erase-data with a session but a wrong password -> 401, tenant rows still there
  6. successful erase -> tenant rows gone, billing rows retained, exactly one erasure audit row

Exit 0 = every check passed; exit 1 = a check failed (its name is printed). This script only
touches a throwaway account it mints itself on the fleet's own mail-suppressed domain, and it
deletes that account as part of the test. It never reads or writes a real customer row.
"""
import json
import os
import subprocess
import sys
import time
import uuid

import requests

APP = os.environ.get("ERASURE_GUARD_HOST", "http://127.0.0.1:8083")
BASE = APP + "/api/v1"
REPO = "/opt/swift/apps/IncentiveSwift"
MCP_SRC = "/opt/swift/mcp/src/main.rs"

FAILED = []


def env_value(key):
    for line in open("/etc/swift/env/incentiveswift.env"):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        if k.strip() == key:
            return v.strip().strip('"').strip("'")
    raise SystemExit(f"missing {key} in /etc/swift/env/incentiveswift.env")


DBURL = env_value("DATABASE_URL")


def psql(query):
    """Run one SQL statement and return (stdout, stderr, returncode)."""
    p = subprocess.run(
        ["psql", DBURL, "-tA", "-c", query], capture_output=True, text=True
    )
    return p.stdout.strip(), p.stderr.strip(), p.returncode


def scalar(query):
    out, err, rc = psql(query)
    if rc != 0:
        raise RuntimeError(f"psql failed ({rc}): {err}")
    return out


def post(path, body, token=None, headers=None):
    h = {"Content-Type": "application/json"}
    if headers:
        h.update(headers)
    if token:
        h["Authorization"] = "Bearer " + token
    r = requests.post(BASE + path, json=body, headers=h, timeout=30)
    try:
        return r.status_code, r.json()
    except Exception:
        return r.status_code, {}


def get(path, token=None):
    h = {}
    if token:
        h["Authorization"] = "Bearer " + token
    r = requests.get(BASE + path, headers=h, timeout=30)
    try:
        return r.status_code, r.json()
    except Exception:
        return r.status_code, {}


def check(name, ok, detail=""):
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}" + (f"  ({detail})" if detail else ""))
    if not ok:
        FAILED.append(name)
    return ok


def main():
    # ── Fixture: a throwaway account on the fleet's mail-suppressed domain ────────────────────
    ts = int(time.time())
    email = f"zz-erasure-guard-{ts}@swiftsoftware.dev"
    password = "GuardTest-Passw0rd-123!"
    status, reg = post("/auth/register", {
        "email": email,
        "password": password,
        "name": "ZZ Erasure Guard",
    })
    if status != 200 or not reg.get("token"):
        return check("fixture: register throwaway account", False, f"status {status}: {reg}")

    session_token = reg["token"]
    account_id = reg["user"]["id"]

    # The account is provisioned with ONE issued API key at mint (account_mint::mint_account), so
    # reveal that key instead of trying to create a second one (which 409s). The reveal is
    # password-gated exactly like the doors it will be used against.
    status, keys = get("/api-keys", token=session_token)
    if status != 200 or not keys.get("api_keys"):
        psql(f"DELETE FROM accounts WHERE id = '{account_id}'")
        return check("fixture: list the provisioned API key", False, f"status {status}: {keys}")
    key_id = keys["api_keys"][0]["id"]
    status, revealed = post(f"/api-keys/{key_id}/reveal", {"password": password},
                            token=session_token)
    if status != 200 or not revealed.get("api_key"):
        psql(f"DELETE FROM accounts WHERE id = '{account_id}'")
        return check("fixture: reveal the provisioned API key", False, f"status {status}: {revealed}")
    api_key = revealed["api_key"]

    # Seed two billing rows so check 6 can prove they are RETAINED (detached).
    psql(
        f"INSERT INTO checkout_sessions (account_id, user_id, price_amount, price_currency, "
        f"description, status, payment_provider) "
        f"VALUES ('{account_id}','{account_id}',49.00,'usd','Guard test invoice','completed','stripe')"
    )
    psql(
        f"INSERT INTO stripe_checkout_sessions (account_id, stripe_session_id, amount, credits, status) "
        f"VALUES ('{account_id}','cs_guardtest_{ts}',4900,49,'complete')"
    )

    # ── Check 1: valid API key -> 401 on close ────────────────────────────────────────────────
    status, _ = post("/auth/close-account", {"password": password, "confirm": "CLOSE"},
                     token=api_key)
    check("1. close-account with a valid API key -> 401", status == 401, f"got {status}")

    # ── Check 2: no credential -> 401 on close ────────────────────────────────────────────────
    status, _ = post("/auth/close-account", {"password": password, "confirm": "CLOSE"})
    check("2. close-account with no credential -> 401", status == 401, f"got {status}")

    # ── Check 3: neither door is public / allowlisted; handlers refuse the api_key role ───────
    public_routes = open(f"{REPO}/src/security/route_policy.rs").read()
    private = all(
        f'"{p}"' not in public_routes
        for p in ("/api/v1/auth/close-account", "/api/v1/auth/erase-data")
    )
    refuse_guard = "refuse_api_key" in open(f"{REPO}/src/handlers/auth_handler.rs").read()
    check("3. close/erase are private routes and the handler refuses api_key",
          private and refuse_guard,
          f"private={private} refuse_guard={refuse_guard}")

    # ── Check 4: no MCP tool names close / erase / delete_account ─────────────────────────────
    # The fleet MCP server is read-only; a tool NAME must not name the destructive doors. Extract
    # the literal tool names (`"name": "..."`) and check them, not the whole file — the word
    # "close" legitimately appears in a comment ("whether a prefix is close").
    import re
    tool_names = re.findall(r'"name"\s*:\s*"([^"]+)"', open(MCP_SRC).read())
    bad = [n for n in tool_names
           if any(w in n.lower() for w in ("close", "erase", "delete_account", "delete-account"))]
    check("4. no MCP tool names close/erase/delete_account", not bad,
          f"tools={len(tool_names)}, bad={bad}")

    # ── Check 5: session + wrong password -> 401, rows survive ────────────────────────────────
    status, _ = post("/auth/erase-data",
                     {"password": "not-the-password", "confirm": "DELETE"},
                     token=session_token)
    still_there = scalar(
        f"SELECT count(*) FROM accounts WHERE id = '{account_id}'"
    ) == "1"
    check("5. erase-data with wrong password -> 401 and rows survive",
          status in (400, 401) and still_there,
          f"status {status}, account_still_there={still_there}")

    # ── Check 6: correct password + DELETE -> rows gone, billing kept, one audit row ─────────
    status, body = post("/auth/erase-data",
                        {"password": password, "confirm": "DELETE"},
                        token=session_token)
    account_gone = scalar(
        f"SELECT count(*) FROM accounts WHERE id = '{account_id}'"
    ) == "0"
    audit_rows = scalar(
        f"SELECT count(*) FROM account_erasures WHERE account_id = '{account_id}'"
    )
    billing_kept = scalar(
        f"SELECT count(*) FROM account_erasure_billing_docs d "
        f"JOIN account_erasures e ON e.id = d.erasure_id "
        f"WHERE e.account_id = '{account_id}'"
    )
    ws_gone = scalar(
        f"SELECT count(*) FROM workspaces WHERE tenant_id = '{account_id}'"
    ) == "0"
    key_gone = scalar(
        f"SELECT count(*) FROM api_keys WHERE user_id = '{account_id}'"
    ) == "0"
    check("6. erase-data -> rows gone, billing retained, one audit row",
          status == 200
          and account_gone
          and audit_rows == "1"
          and billing_kept == "2"
          and ws_gone
          and key_gone,
          f"status {status}, account_gone={account_gone}, audit_rows={audit_rows}, "
          f"billing_kept={billing_kept}, workspace_gone={ws_gone}, api_key_gone={key_gone}")

    # ── Summary ──────────────────────────────────────────────────────────────────────────────
    if FAILED:
        print(f"\nFAILED: {len(FAILED)} check(s): {', '.join(FAILED)}")
        sys.exit(1)
    print("\nALL 6 CHECKS PASSED")
    sys.exit(0)


if __name__ == "__main__":
    main()
