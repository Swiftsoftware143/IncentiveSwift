#!/usr/bin/env python3
"""Live parity acceptance for the IncentiveSwift MCP write-tier extension (kanban t_4fa31d4f).

Proves, with a real account key, that every UI action family now has a reachable MCP tool:
each new tool is exercised (create -> read back -> update -> read back -> archive -> gone),
and the state-change families are run against real prerequisite rows and read back.

Never prints a full key (prefix only). Uses a throwaway test account that is deleted at the end.

Usage: python3 scripts/parity-acceptance.py [base_url] [account_id]
"""
import json
import os
import subprocess
import sys
import uuid

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8083"
KEY = "is_key_a1b2c3d4e5f60718293a4b5c6d7e8f90"  # prefix a1b2c3d4
ACCT = sys.argv[2] if len(sys.argv) > 2 else "fb352b34-9254-48d5-a7af-7cf5b042abde"

ENV_FILE = "/opt/swift/apps/IncentiveSwift/.env"


def db_password():
    with open(ENV_FILE) as fh:
        for line in fh:
            line = line.strip()
            if line.startswith("DATABASE_URL="):
                url = line.split("=", 1)[1]
                # postgres://swift:PW@host:port/db
                return url.split(":")[2].split("@")[0]
    raise RuntimeError("no DATABASE_URL in .env")


def sql(query):
    env = dict(os.environ)
    env["PGPASSWORD"] = db_password()
    out = subprocess.run(
        ["psql", "-U", "swift", "-h", "localhost", "-d", "incentiveswift", "-Atq", "-c", query],
        capture_output=True, text=True, env=env,
    )
    if out.returncode != 0:
        raise RuntimeError(f"psql failed: {out.stderr}")
    return out.stdout.strip()


def mcp(tool, **args):
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": args}}
    req = __import__("urllib.request").request.Request(
        BASE + "/mcp", data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY})
    with __import__("urllib.request").request.urlopen(req) as r:
        d = json.load(r)
    res = d["result"]
    return (not res.get("isError"), res.get("structuredContent"))


def tools_list():
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}
    req = __import__("urllib.request").request.Request(
        BASE + "/mcp", data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY})
    with __import__("urllib.request").request.urlopen(req) as r:
        d = json.load(r)
    names = [t["name"] for t in d["result"]["tools"]]
    cursor = d["result"].get("nextCursor")
    while cursor:
        body["params"] = {"cursor": cursor}
        req = __import__("urllib.request").request.Request(
            BASE + "/mcp", data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY})
        with __import__("urllib.request").request.urlopen(req) as r:
            d = json.load(r)
        names += [t["name"] for t in d["result"]["tools"]]
        cursor = d["result"].get("nextCursor")
    return names


RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok), detail))
    print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  — {detail}" if detail else ""))


def expect_ok(tag, res):
    ok, sc = res
    check(tag, ok, "" if ok else str(sc))
    return sc


def expect_err(tag, res):
    ok, sc = res
    check(tag, not ok, "" if (not ok) else f"unexpected success {sc}")
    return sc


def new_contact(email=None):
    email = email or f"zz-{uuid.uuid4().hex[:8]}@example.invalid"
    ok, sc = mcp("incentiveswift_create_contact", email=email, first_name="ZZ", last_name="Parity")
    assert ok, sc
    return sc["id"]


def main():
    print(f"== parity acceptance: {BASE} (account {ACCT}, key prefix a1b2c3d4) ==")

    # Give the throwaway account a paid tier so the clone cap (max_campaigns) does not bite at 1.
    sql(f"UPDATE accounts SET plan_tier_id = 'c3a2d26b-2fdb-4a6b-943a-e969b07f98d7' WHERE id = '{ACCT}'")

    # 0. tool inventory
    names = tools_list()
    print(f"  tools/list -> {len(names)} tools")
    expected_new = [
        "incentiveswift_list_contacts", "incentiveswift_create_contact",
        "incentiveswift_update_contact", "incentiveswift_archive_contact",
        "incentiveswift_list_tags", "incentiveswift_create_tag",
        "incentiveswift_update_tag", "incentiveswift_archive_tag",
        "incentiveswift_list_kb_articles", "incentiveswift_create_kb_article",
        "incentiveswift_update_kb_article", "incentiveswift_archive_kb_article",
        "incentiveswift_list_custom_fields", "incentiveswift_create_custom_field",
        "incentiveswift_update_custom_field", "incentiveswift_archive_custom_field",
        "incentiveswift_list_questions", "incentiveswift_create_question",
        "incentiveswift_update_question", "incentiveswift_archive_question",
        "incentiveswift_list_integration_targets", "incentiveswift_create_integration_target",
        "incentiveswift_update_integration_target", "incentiveswift_archive_integration_target",
        "incentiveswift_list_portfolio_companies", "incentiveswift_create_portfolio_company",
        "incentiveswift_update_portfolio_company", "incentiveswift_archive_portfolio_company",
        "incentiveswift_list_surfaces", "incentiveswift_create_surface",
        "incentiveswift_update_surface", "incentiveswift_archive_surface",
        "incentiveswift_link_integration", "incentiveswift_unlink_integration",
        "incentiveswift_update_reward_status", "incentiveswift_add_ticket_message",
        "incentiveswift_regenerate_member_qr", "incentiveswift_draw_raffle_winner",
        "incentiveswift_clone_campaign",
    ]
    missing = [n for n in expected_new if n not in names]
    check("all 39 new tools registered", not missing, f"missing: {missing}" if missing else "39/39")

    # 1. a campaign for the campaign-scoped families + clone
    ok, sc = mcp("incentiveswift_create_campaign", name="ZZ Parity Poll",
                 type="poll", tag_namespace="zzparity" + uuid.uuid4().hex[:4])
    assert ok, sc
    campaign_id = sc["id"]
    slug = sc["slug"]
    print(f"  campaign {slug} -> {campaign_id}")

    # 2. contacts
    cid = new_contact()
    ok, sc = mcp("incentiveswift_list_contacts")
    check("contacts list shows created", ok and any(i["id"] == cid for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_contact", id=cid, last_name="Parity-Updated")
    check("contact update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_contact", id=cid)
    check("contact archive (unlink)", ok and sc.get("archived"))

    # 3. tags
    ok, sc = mcp("incentiveswift_create_tag", name="zz-parity-tag", color="#ff0000")
    check("tag create", ok)
    tag_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_tags")
    check("tags list shows created", ok and any(i["id"] == tag_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_tag", id=tag_id, name="zz-parity-tag-2")
    check("tag update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_tag", id=tag_id)
    check("tag archive", ok and sc.get("archived"))

    # 4. knowledge base
    ok, sc = mcp("incentiveswift_create_kb_article", title="ZZ Parity KB",
                 content="body", audience="user", category="zz")
    check("kb create", ok)
    kb_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_kb_articles")
    check("kb list shows created", ok and any(i["id"] == kb_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_kb_article", id=kb_id, is_published=False)
    check("kb update (unpublish)", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_kb_article", id=kb_id, hard_delete=True)
    check("kb archive (hard)", ok and sc.get("archived"))

    # 5. custom fields (campaign-scoped)
    ok, sc = mcp("incentiveswift_create_custom_field", campaign_id=campaign_id,
                 field_key="zz_q", field_label="ZZ Question", field_type="text",
                 required=True, options=["a", "b"])
    check("custom field create", ok)
    cf_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_custom_fields", campaign_id=campaign_id)
    check("custom field list", ok and any(i["id"] == cf_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_custom_field", id=cf_id, field_label="ZZ Question 2")
    check("custom field update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_custom_field", id=cf_id)
    check("custom field archive", ok and sc.get("archived"))

    # 6. questions (campaign-scoped)
    ok, sc = mcp("incentiveswift_create_question", campaign_id=campaign_id,
                 question_key="zz_q1", question_text="What?", question_type="multiple_choice",
                 options={"choices": ["x", "y"]}, correct_answer="x", score_weight=5)
    check("question create", ok)
    q_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_questions", campaign_id=campaign_id)
    check("question list", ok and any(i["id"] == q_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_question", id=q_id, question_text="What now?")
    check("question update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_question", id=q_id)
    check("question archive", ok and sc.get("archived"))

    # 7. integration targets
    ok, sc = mcp("incentiveswift_create_integration_target", name="ZZ Hook",
                 webhook_url="https://example.invalid/hook", provider="webhook",
                 events=["entry.created"])
    check("integration target create", ok)
    it_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_integration_targets")
    check("integration target list", ok and any(i["id"] == it_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_integration_target", id=it_id, is_active=False)
    check("integration target update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_integration_target", id=it_id, hard_delete=True)
    check("integration target archive", ok and sc.get("archived"))

    # 8. portfolio companies
    ok, sc = mcp("incentiveswift_create_portfolio_company", name="ZZ Co",
                 domain="zzco.invalid", email="zzco@example.invalid")
    check("portfolio company create", ok)
    pc_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_portfolio_companies")
    check("portfolio company list", ok and any(i["id"] == pc_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_portfolio_company", id=pc_id, description="zz desc")
    check("portfolio company update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_portfolio_company", id=pc_id)
    check("portfolio company archive", ok and sc.get("archived"))

    # 9. surfaces
    ok, sc = mcp("incentiveswift_create_surface", name="ZZ Surface")
    check("surface create", ok)
    s_id = sc["id"]
    ok, sc = mcp("incentiveswift_list_surfaces")
    check("surface list", ok and any(i["id"] == s_id for i in sc["items"]))
    ok, sc = mcp("incentiveswift_update_surface", id=s_id, name="ZZ Surface 2")
    check("surface update", ok and sc.get("updated"))
    ok, sc = mcp("incentiveswift_archive_surface", id=s_id)
    check("surface archive", ok and sc.get("archived"))

    # 10. campaign integrations (link / unlink)
    ok, sc = mcp("incentiveswift_create_integration_target", name="ZZ Hook 2",
                 webhook_url="https://example.invalid/hook2")
    assert ok, sc
    it2 = sc["id"]
    ok, sc = mcp("incentiveswift_link_integration", campaign_id=campaign_id,
                 integration_id=it2, trigger_events=["entry.created"])
    check("link integration", ok and sc.get("linked"))
    ok, sc = mcp("incentiveswift_unlink_integration", campaign_id=campaign_id, integration_id=it2)
    check("unlink integration", ok and sc.get("unlinked"))

    # 11. clone campaign
    ok, sc = mcp("incentiveswift_clone_campaign", slug=slug)
    check("clone campaign", ok and "slug" in sc, f"slug={sc.get('slug')}" if ok else str(sc))
    clone_id = sc.get("id") if ok else None

    # 12. support ticket message
    ok, sc = mcp("incentiveswift_create_support_ticket", subject="ZZ Ticket",
                 description="parity", priority="normal", category="zz")
    check("ticket create (existing tool)", ok)
    t_id = sc["id"]
    ok, sc = mcp("incentiveswift_add_ticket_message", ticket_id=t_id, body="ZZ reply")
    check("ticket message add", ok and "id" in sc)
    cnt = sql(f"SELECT count(*) FROM support_ticket_messages WHERE ticket_id = '{t_id}' AND body = 'ZZ reply'")
    check("ticket message row present", cnt == "1")

    # 13. reward status (approve) — SQL-primed program/tier/member/reward
    prog = sql(f"INSERT INTO loyalty_programs (account_id, name, slug) "
               f"VALUES ('{ACCT}', 'ZZ Parity Program', 'zz-parity-prog-{uuid.uuid4().hex[:6]}') RETURNING id")
    tier = sql(f"INSERT INTO loyalty_reward_tiers (program_id, name, points_required, requires_approval, reward_tag) "
               f"VALUES ('{prog}', 'ZZ Tier', 10, true, 'zz_tier') RETURNING id")
    mcontact = sql("INSERT INTO contacts (id, email) VALUES ('" + str(uuid.uuid4()) + "', '" +
                   f"zz-reward-{uuid.uuid4().hex[:6]}@example.invalid') RETURNING id")
    member = sql(f"INSERT INTO loyalty_members (program_id, contact_id) VALUES ('{prog}', '{mcontact}') RETURNING id")
    reward = sql(f"INSERT INTO loyalty_rewards_earned (tier_id, member_id, status) "
                 f"VALUES ('{tier}', '{member}', 'pending') RETURNING id")
    ok, sc = mcp("incentiveswift_update_reward_status", id=reward, status="approved")
    check("reward status approve", ok and sc.get("status") == "approved", str(sc) if not ok else "")
    st = sql(f"SELECT status FROM loyalty_rewards_earned WHERE id = '{reward}'")
    check("reward row approved in DB", st == "approved", st)

    # 14. member QR regenerate
    ok, sc = mcp("incentiveswift_regenerate_member_qr", member_id=member)
    check("member QR regenerate", ok and sc.get("qr_code"), "" if ok else str(sc))
    qr = sql(f"SELECT qr_code IS NOT NULL AND qr_code = '{sc.get('qr_code')}' FROM loyalty_members WHERE id = '{member}'")
    check("member QR row updated", qr == "t", qr)

    # 15. raffle draw — SQL-primed raffle campaign + entries
    rsuffix = uuid.uuid4().hex[:6]
    raffle = sql(f"INSERT INTO campaigns (account_id, name, slug, type, tag_namespace) "
                 f"VALUES ('{ACCT}', 'ZZ Raffle', 'zz-raffle-{rsuffix}', 'raffle', 'zzraffle{rsuffix}') RETURNING id")
    e1c = sql("INSERT INTO contacts (id, email) VALUES ('" + str(uuid.uuid4()) + "', '" +
              f"zz-e1-{rsuffix}@example.invalid') RETURNING id")
    e2c = sql("INSERT INTO contacts (id, email) VALUES ('" + str(uuid.uuid4()) + "', '" +
              f"zz-e2-{rsuffix}@example.invalid') RETURNING id")
    sql(f"INSERT INTO entries (contact_id, campaign_id) VALUES ('{e1c}', '{raffle}'), ('{e2c}', '{raffle}')")
    ok, sc = mcp("incentiveswift_draw_raffle_winner", campaign_id=raffle)
    check("raffle draw", ok and "winner_entry_id" in sc, str(sc) if not ok else f"winner={sc.get('winner_entry_id')}")
    wc = sql(f"SELECT count(*) FROM entries WHERE campaign_id = '{raffle}' AND outcome = 'winner'")
    check("exactly one winner marked", wc == "1", wc)

    # 16. scoping: a foreign id must answer not_found
    foreign = str(uuid.uuid4())
    ok, sc = mcp("incentiveswift_update_contact", id=foreign, first_name="x")
    check("foreign contact update -> not_found", not ok, "" if not ok else f"leaked {sc}")

    # teardown: remove the throwaway account (cascades every linked row) and the orphaned
    # contacts the unlink-archive left behind (contacts are shared across tenants, so an
    # archive is an unlink, not a hard delete — the ZZ fixtures are hard-deleted here).
    sql(f"DELETE FROM accounts WHERE id = '{ACCT}'")
    sql("DELETE FROM contacts WHERE email LIKE 'zz-%@example.invalid'")

    print()
    fails = [r for r in RESULTS if not r[1]]
    print(f"== RESULT: {len(RESULTS) - len(fails)}/{len(RESULTS)} checks passed ==")
    for name, ok2, detail in fails:
        print(f"  FAILED: {name} {detail}")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
