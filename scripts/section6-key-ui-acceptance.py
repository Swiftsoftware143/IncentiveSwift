#!/usr/bin/env python3
"""Live acceptance — plan §6 key-UI rules on IncentiveSwift (card t_808a9370).

docs/fleet-per-account-keys-and-mcp-2026-11-25.md §6 (David, 2026-11-25): private-by-default keys,
View/Copy as SEPARATE controls, Copy fetches-and-clipboards without rendering, the API Keys screen is
the rotate/revoke home, the console MCP screen's key card is FIRST with View+Copy ONLY (no
rotate/regenerate), and every new account auto-mints ONE key labelled `MCP — <account>` in the
account's own transaction.

Checks (one PASS/FAIL line each; exit 1 on any FAIL):
  1. fresh signup holds EXACTLY one live key labelled `MCP — <account>` (SQL)
  2. GET key list with the console session -> 200, masked only (no full secret in the body)
  3. Copy path: POST reveal (session, no body) -> secret that answers tools/list 200 at POST /mcp
  4. View path: POST reveal -> 200 same secret AND exactly one new api_key_reveals audit row
  5. rotate (POST regenerate) -> old secret tools/list 401, new secret tools/list 200
  6. real-Chromium DOM assertion on the served console MCP screen: key card FIRST, View+Copy present,
     no button/link text matching /rotate|regenerate/i, no regen/rotate handler in the Mcp body
  7. teardown: probe account erased, residue 0

Never prints a full key — prefixes only.

Usage: python3 scripts/section6-key-ui-acceptance.py [base_url]
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from urllib.parse import unquote, urlparse

BASE = (sys.argv[1] if len(sys.argv) > 1 else "https://app.incentiveswift.com").rstrip("/")
APP_ENV = Path("/etc/swift/env/incentiveswift.env")
HARNESS = "t_808a9370"

FAILED = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ((" — " + detail) if detail else ""))
    if not ok:
        FAILED.append(name)


def http(method, path, body=None, token=None, api_key=None, extra_headers=None, raw_body=False):
    url = BASE + path
    if body is None:
        data = b"" if raw_body else None
    else:
        data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    req.add_header("User-Agent", "swift-section6-acceptance")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    if api_key:
        req.add_header("Authorization", "Bearer " + api_key)
    for k, v in (extra_headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read().decode(errors="replace")
            try:
                return r.status, (json.loads(raw) if raw else {})
            except json.JSONDecodeError:
                return r.status, raw
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, (json.loads(raw) if raw else {})
        except json.JSONDecodeError:
            return e.code, raw
    except urllib.error.URLError as e:
        raise SystemExit(f"cannot reach {url}: {e}")


def _dsn():
    for line in APP_ENV.read_text().splitlines():
        if line.startswith("DATABASE_URL="):
            return line.split("=", 1)[1].strip().strip('"')
    raise SystemExit(f"no DATABASE_URL in {APP_ENV}")


def sql(query):
    u = urlparse(_dsn())
    db = unquote(u.path).lstrip("/") or "incentiveswift"
    r = subprocess.run(
        ["docker", "exec", "-i", "-e", f"PGPASSWORD={unquote(u.password or '')}",
         "swift-postgres", "psql", "-U", unquote(u.username or "swift"), "-d", db,
         "-tAF", "|", "-v", "ON_ERROR_STOP=1", "-c", query],
        capture_output=True, text=True, timeout=60,
    )
    if r.returncode != 0:
        raise SystemExit(f"SQL failed: {r.stderr.strip()[:400]}\n--- {query[:200]}")
    return [line.split("|") for line in r.stdout.splitlines() if line.strip()]


def scalar(query):
    rows = sql(query)
    return rows[0][0] if rows else None


def mcp_tools_list(api_key):
    s, body = http("POST", "/mcp",
                   {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}, api_key=api_key)
    tools = []
    if s == 200 and isinstance(body, dict) and isinstance(body.get("result"), dict):
        tools = body["result"].get("tools") or []
    return s, tools


NODE_SCRIPT = r"""
const puppeteer = require('/usr/lib/node_modules/puppeteer');
(async () => {
  const token = process.env.IS_TOKEN;
  const user = JSON.parse(process.env.IS_USER || '{}');
  const browser = await puppeteer.launch({
    executablePath: '/usr/bin/chromium',
    headless: 'new',
    args: ['--no-sandbox', '--disable-setuid-sandbox', '--disable-dev-shm-usage']
  });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(String(e)));
    await page.goto('https://admin.incentiveswift.com/', { waitUntil: 'domcontentloaded', timeout: 30000 });
    await page.evaluate((t, u) => {
      sessionStorage.setItem('is_token', t);
      sessionStorage.setItem('is_user', JSON.stringify(u));
    }, token, user);
    await page.goto('https://admin.incentiveswift.com/', { waitUntil: 'networkidle2', timeout: 30000 });

    // Click the MCP nav button.
    const clicked = await page.evaluate(() => {
      const btns = [...document.querySelectorAll('button.nav-item, aside button, nav button')];
      const mcp = btns.find(b => /^MCP$/.test((b.textContent || '').trim()) || b.getAttribute('aria-label') === 'MCP');
      if (!mcp) return false;
      mcp.click();
      return true;
    });

    // Wait for the key card's View + Copy buttons (they render only once keys load).
    let view = null, copy = null;
    for (let i = 0; i < 40; i++) {
      const r = await page.evaluate(() => {
        const card = document.getElementById('mcp-key-card');
        if (!card) return null;
        const btns = [...card.querySelectorAll('button')];
        const view = btns.find(b => (b.textContent || '').trim() === 'View');
        const copy = btns.find(b => (b.textContent || '').trim() === 'Copy');
        return { view: !!view, copy: !!copy };
      });
      if (r && r.view && r.copy) { view = r.view; copy = r.copy; break; }
      await new Promise(res => setTimeout(res, 250));
    }

    const dom = await page.evaluate(() => {
      const cards = [...document.querySelectorAll('.card')];
      const keyCard = document.getElementById('mcp-key-card');
      let firstIsKey = false;
      if (keyCard) {
        // first .card in DOM order among cards that share the key card's content area
        const keyIdx = cards.indexOf(keyCard);
        // the first .card that is the key card OR appears after it but before the connect card
        firstIsKey = keyIdx >= 0 && cards.slice(0, keyIdx).every(c => !c.querySelector('button'));
      }
      const banned = [];
      for (const el of document.querySelectorAll('button, a, [role=button]')) {
        const t = (el.textContent || '').trim();
        if (/rotate|regenerate/i.test(t)) banned.push({ tag: el.tagName, text: t.slice(0, 60) });
      }
      return {
        firstCardIsKeyCard: keyCard ? (cards[0] === keyCard) : false,
        keyCardPresent: !!keyCard,
        keyCardHasView: !!keyCard && [...keyCard.querySelectorAll('button')].some(b => (b.textContent || '').trim() === 'View'),
        keyCardHasCopy: !!keyCard && [...keyCard.querySelectorAll('button')].some(b => (b.textContent || '').trim() === 'Copy'),
        bannedControls: banned,
        cardsCount: cards.length,
        firstCardId: cards[0] ? cards[0].id : null,
      };
    });

    // "no handler named regen/rotate on the Mcp view": slice the inline script's Mcp body.
    const source = await page.evaluate(() => {
      const scripts = [...document.querySelectorAll('script')];
      for (const s of scripts) {
        const text = s.textContent || '';
        const start = text.indexOf('const Mcp = () => {');
        if (start < 0) continue;
        const end = text.indexOf('const PaymentProviders = () => {');
        if (end < 0 || end <= start) return text.slice(start);
        return text.slice(start, end);
      }
      return null;
    });

    console.log(JSON.stringify({
      clickedMcpNav: clicked,
      view, copy, dom,
      mcpBodyHasRotateOrRegen: source === null ? null : /rotate|regenerate/i.test(source),
      pageErrors: errors,
    }));
  } finally {
    await browser.close();
  }
})().catch(e => { console.error('NODE-ERR', e && e.message); process.exit(1); });
"""


def dom_assertion(probe):
    env = dict(os.environ)
    env["IS_TOKEN"] = probe["token"]
    env["IS_USER"] = json.dumps({"id": probe["user_id"], "email": probe["email"],
                                 "name": probe["name"], "role": "company_admin", "plan": "free"})
    node_file = Path("/root/.hermes/profiles/builds/cache/scratch/is-dom-assert.cjs")
    node_file.write_text(NODE_SCRIPT)
    r = subprocess.run(
        ["node", str(node_file)], capture_output=True, text=True, timeout=120, env=env,
    )
    out = r.stdout.strip()
    if r.returncode != 0:
        return False, f"node rc={r.returncode} stderr={r.stderr.strip()[:400]}"
    for line in out.splitlines():
        if line.startswith("{"):
            try:
                return True, json.loads(line)
            except json.JSONDecodeError:
                return True, line
    return False, f"no JSON output: {out[:400]}"


def register_probe(tag):
    email = f"zz-is6-keyui-{tag}@example.invalid"
    password = "Section6Probe!2026"
    name = f"ZZ IS6 KeyUI {tag}"
    s, b = http("POST", "/api/v1/auth/register", {
        "name": name,
        "email": email,
        "password": password,
        "workspace_name": f"ZZ IS6 Workspace {tag}",
    }, extra_headers={"X-Swift-Harness": HARNESS})
    if s not in (200, 201):
        raise SystemExit(f"probe signup failed: {s} {b}")
    b = b if isinstance(b, dict) else {}
    user = b.get("user") or {}
    return {
        "email": email,
        "password": password,
        "name": name,
        "token": b["token"],
        "user_id": user.get("id"),
    }


def list_keys(p):
    s, b = http("GET", "/api/v1/api-keys", token=p["token"])
    keys = (b or {}).get("api_keys") or [] if s == 200 else []
    return s, keys


def erase(p):
    s, _ = http("POST", "/api/v1/auth/erase-data",
                {"password": p["password"], "confirm": "DELETE"}, token=p["token"])
    return s


def main():
    tag = uuid.uuid4().hex[:8]
    a = register_probe(tag)
    print(f"probe account {a['name']}  user {a['user_id']}")

    # ── 1. exactly ONE live key labelled `MCP — <account>` ─────────────────────────────────────
    nkeys = scalar(f"SELECT COUNT(*) FROM api_keys WHERE user_id = '{a['user_id']}' AND is_active = true")
    keyname = scalar(f"SELECT name FROM api_keys WHERE user_id = '{a['user_id']}' AND is_active = true LIMIT 1")
    expect = f"MCP — {a['name']}"
    check("1 signup auto-minted EXACTLY one key labelled `MCP — <account>`",
          nkeys == "1" and keyname == expect,
          f"keys={nkeys} name={keyname!r} (expect {expect!r})")

    # ── 2. list is masked ───────────────────────────────────────────────────────────────────────
    s, keys = list_keys(a)
    k = keys[0] if keys and isinstance(keys[0], dict) else None
    has_plaintext = any(("api_key" in kk) or ("key_sealed" in kk) or ("key_hash" in kk) for kk in keys) if isinstance(keys, list) else False
    check("2 GET key list with the console session -> 200, masked only (no full secret)",
          s == 200 and len(keys) == 1 and not has_plaintext and bool(k and k.get("prefix")),
          f"HTTP {s} keys={len(keys)} plaintext={has_plaintext}")

    # ── 3. Copy path: reveal (session, no body) -> secret that opens the /mcp door ─────────────
    s, body = http("POST", f"/api/v1/api-keys/{k['id']}/reveal", token=a["token"], raw_body=True)
    secret = body.get("api_key") if isinstance(body, dict) else None
    m3, tools3 = mcp_tools_list(secret) if isinstance(secret, str) else (None, [])
    check("3 Copy path: POST reveal (no body) -> secret that answers tools/list 200 at POST /mcp",
          s == 200 and isinstance(secret, str) and secret.startswith("is_key_") and m3 == 200 and len(tools3) > 0,
          f"reveal HTTP {s} tools/list HTTP {m3} tools={len(tools3)}")

    # ── 4. View path: reveal -> same secret + exactly ONE new audit row ─────────────────────────
    before = int(scalar(f"SELECT COUNT(*) FROM api_key_reveals WHERE key_id = '{k['id']}'") or 0)
    s4, body4 = http("POST", f"/api/v1/api-keys/{k['id']}/reveal", token=a["token"], raw_body=True)
    secret4 = body4.get("api_key") if isinstance(body4, dict) else None
    after = int(scalar(f"SELECT COUNT(*) FROM api_key_reveals WHERE key_id = '{k['id']}'") or 0)
    who_when = sql(
        f"SELECT user_id::text, revealed_at::text FROM api_key_reveals "
        f"WHERE key_id = '{k['id']}' ORDER BY revealed_at DESC LIMIT 1")
    who = who_when[0][0] if who_when else None
    when = who_when[0][1] if who_when and len(who_when[0]) > 1 else None
    check("4 View path: POST reveal -> 200 same secret AND exactly one new audit row (who/key/when)",
          s4 == 200 and secret4 == secret and (after - before) == 1 and who == a["user_id"] and bool(when),
          f"reveal HTTP {s4} same={secret4 == secret} audit delta={after - before} who={who} when={bool(when)}")

    # ── 5. rotate -> old 401, new 200, and the console session survives ────────────────────────
    s5, body5 = http("POST", f"/api/v1/api-keys/{k['id']}/regenerate", {}, token=a["token"])
    new_secret = body5.get("api_key") if isinstance(body5, dict) else None
    m_old, _ = mcp_tools_list(secret) if isinstance(secret, str) else (None, [])
    m_new, t_new = mcp_tools_list(new_secret) if isinstance(new_secret, str) else (None, [])
    # the JWT session is untouched by a rotate: an authenticated read still answers 200
    sm, _ = http("GET", "/api/v1/auth/me", token=a["token"])
    check("5 rotate (POST regenerate) -> old secret tools/list 401, new secret tools/list 200, session untouched",
          s5 in (200, 201) and isinstance(new_secret, str) and new_secret.startswith("is_key_")
          and m_old == 401 and m_new == 200 and len(t_new) > 0 and sm == 200,
          f"rotate HTTP {s5} old={m_old} new={m_new} tools={len(t_new)} session={sm}")

    # ── 6. real-Chromium DOM assertion on the served console MCP screen ────────────────────────
    ok6, dom = dom_assertion(a)
    if ok6 and isinstance(dom, dict):
        d = dom.get("dom") or {}
        source_bad = dom.get("mcpBodyHasRotateOrRegen")
        passed = (dom.get("clickedMcpNav") is True
                  and d.get("keyCardPresent") is True
                  and d.get("firstCardIsKeyCard") is True
                  and d.get("keyCardHasView") is True
                  and d.get("keyCardHasCopy") is True
                  and (d.get("bannedControls") or []) == []
                  and source_bad is False)
        check("6 DOM: key card first, View+Copy present, no rotate/regenerate control or handler",
              passed, json.dumps({k_: v for k_, v in dom.items() if k_ != "pageErrors"})[:600])
        print("     DOM OUTPUT: " + json.dumps(dom)[:1500])
    else:
        check("6 DOM assertion ran", False, str(dom)[:400])

    # ── 7. teardown ─────────────────────────────────────────────────────────────────────────────
    es = erase(a)
    if a["user_id"]:
        sql(f"DELETE FROM account_erasures WHERE account_id = '{a['user_id']}'")
    residue = int(scalar(f"SELECT COUNT(*) FROM accounts WHERE id = '{a['user_id']}'") or 0)
    residue_keys = int(scalar(f"SELECT COUNT(*) FROM api_keys WHERE user_id = '{a['user_id']}'") or 0)
    residue_reveals = int(scalar(f"SELECT COUNT(*) FROM api_key_reveals WHERE key_id = '{k['id']}'") or 0)
    check("7 probe account erased, residue 0",
          es == 200 and residue == 0 and residue_keys == 0 and residue_reveals == 0,
          f"erase={es} accounts={residue} api_keys={residue_keys} api_key_reveals={residue_reveals}")

    print()
    if FAILED:
        print(f"{len(FAILED)} CHECK(S) FAILED: " + ", ".join(FAILED))
        return 1
    print("ALL 7 CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
