#!/usr/bin/env python3
"""Re-scope AITER COMMERCE milestones to a single-weekend build.
Consolidates 7 release-themed milestones into 5 time-boxed weekend milestones
(Day 1 / Day 2 / post-weekend backlog), reassigning all open issues, then
deletes the old milestones."""
import subprocess, json

GH = "/opt/data/home/.local/bin/gh"
REPO = "DeathSurfing/aiter-commerce"

def gh(*a):
    r = subprocess.run([GH, "api", *a], capture_output=True, text=True)
    if r.returncode != 0:
        print("API ERR:", r.stderr.strip()[:150])
        return None
    return r.stdout

# (title, description, due_on or "")
NEW_MILESTONES = [
    ("Day 1 · Foundation & Agent Catalog",
     "Core data model (Amount, Product, Cart, CheckoutSession, Order, Merchant) + agent-readable "
     "catalog/discovery (feed, search, /.well-known/agent-card.json, llms.txt, seed catalog). Gate for everything else.",
     "2026-08-22T12:00:00Z"),
    ("Day 1 · Checkout Flow",
     "Cart + checkout session APIs, totals computation, state machine + idempotency, storage trait.",
     "2026-08-22T20:00:00Z"),
    ("Day 2 · Razorpay Payments Rail",
     "Razorpay Orders client + signing, payment-link generation, webhook HMAC verification, paid "
     "reconciliation, UPI Reserve Pay consent + agent debit, sandbox config.",
     "2026-08-23T12:00:00Z"),
    ("Day 2 · Trust, MCP & End-to-End Demo",
     "Agent identity + request signing, verification middleware, spend caps, receipts/audit, MCP "
     "surface, example agent client, end-to-end integration test, demo merchant + README.",
     "2026-08-23T20:00:00Z"),
    ("Backlog · Post-weekend hardening",
     "Out of weekend scope. Persistence, tracing/metrics, external config + CLI, rate limiting, "
     "security pass.",
     ""),
]

OLD_TO_NEW = {
    "v0.1.0 - Core Data Model & Foundations": "Day 1 · Foundation & Agent Catalog",
    "v0.2.0 - Agent-Facing Catalog & Discovery": "Day 1 · Foundation & Agent Catalog",
    "v0.3.0 - Cart & Checkout Flow": "Day 1 · Checkout Flow",
    "v0.4.0 - Payments Rail: Razorpay": "Day 2 · Razorpay Payments Rail",
    "v0.5.0 - Agent Identity & Trust": "Day 2 · Trust, MCP & End-to-End Demo",
    "v0.6.0 - MCP Surface & End-to-End Demo": "Day 2 · Trust, MCP & End-to-End Demo",
    "v1.0.0 - Hardening & Production Readiness": "Backlog · Post-weekend hardening",
}

# capture old milestone numbers before deleting
old_all = gh(f"repos/{REPO}/milestones?state=all")
old_by_title = {m["title"]: m["number"] for m in json.loads(old_all or "[]")}

# create new milestones
new_by_title = {}
for t, d, due in NEW_MILESTONES:
    cmd = [GH, "api", "--method", "POST", f"repos/{REPO}/milestones", "-f", f"title={t}", "-f", f"description={d}"]
    if due:
        cmd += ["-f", f"due_on={due}"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode == 0:
        new_by_title[t] = json.loads(r.stdout)["number"]
        print("milestone created:", t, "#", new_by_title[t])
    else:
        for m in json.loads(gh(f"repos/{REPO}/milestones?state=all") or "[]"):
            if m["title"] == t:
                new_by_title[t] = m["number"]
        print("milestone exists:", t, "#", new_by_title.get(t))

# reassign open issues
issues = json.loads(gh(f"repos/{REPO}/issues?state=open&per_page=100") or "[]")
moved = 0
for i in issues:
    ms = i.get("milestone")
    if not ms:
        continue
    newt = OLD_TO_NEW.get(ms["title"])
    nm = new_by_title.get(newt) if newt else None
    if not nm:
        print("no target for", i["number"], ms["title"]); continue
    subprocess.run([GH, "api", "--method", "PATCH", f"repos/{REPO}/issues/{i['number']}",
                    "-f", f"milestone={nm}"], capture_output=True, text=True)
    moved += 1
    print(f"moved #{i['number']} -> {newt}")

# delete old milestones
for t, num in old_by_title.items():
    r = subprocess.run([GH, "api", "--method", "DELETE", f"repos/{REPO}/milestones/{num}"],
                       capture_output=True, text=True)
    print("delete old:", t, "(was #%s):" % num, "ok" if r.returncode == 0 else r.stderr.strip()[:120])

print("\n=== NEW MILESTONE MAP ===")
print(json.dumps(new_by_title, indent=2))
print("moved issues:", moved)
