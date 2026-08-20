#!/usr/bin/env python3
"""One-shot planner: creates milestones, labels, and issues for AITER COMMERCE,
mirroring the featrs milestone/issue structure (version-themed milestones with
due dates + granular enhancement issues with help-wanted and domain labels)."""
import subprocess, json, sys

GH = "/opt/data/home/.local/bin/gh"
REPO = "DeathSurfing/aiter-commerce"

def gh(*args):
    r = subprocess.run([GH, "api", *args], capture_output=True, text=True)
    if r.returncode != 0:
        print("FAILED:", r.stderr.strip()[:300])
        return None
    return r.stdout

def gh_label(*args):
    r = subprocess.run([GH, "label", "create", *args, "--repo", REPO],
                       capture_output=True, text=True)
    return r.returncode == 0, (r.stderr or r.stdout).strip()[:200]

# ----- (title, description, due_on) -----
MILESTONES = [
    ("v0.1.0 - Core Data Model & Foundations",
     "Serde data types for the whole commerce flow in dep-free aiter-core: Amount/Money, "
     "Product, Cart, CheckoutSession, Order, Merchant identity. Foundation for everything else.",
     "2026-09-10T12:00:00Z"),
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "Let any agent find and understand the catalog: REST feed endpoint, product search/lookup, "
     "merchant discovery profile (/.well-known/agent-card.json), llms.txt-style export, seed demo catalog.",
     "2026-09-25T12:00:00Z"),
    ("v0.3.0 - Cart & Checkout Flow",
     "End-to-end session state: create/update cart, compute totals, pick fulfillment, submit "
     "checkout session via a clear state machine over a storage trait.",
     "2026-10-10T12:00:00Z"),
    ("v0.4.0 - Payments Rail: Razorpay",
     "Wire checkout to Razorpay: Orders API client + signing, payment-link generation, webhook "
     "HMAC verification, order-paid reconciliation, and the UPI Reserve Pay consent + agent-"
     "debit flow. Sandbox mode first.",
     "2026-10-30T12:00:00Z"),
    ("v0.5.0 - Agent Identity & Trust",
     "Prove an agent is authorized to transact: signed requests (RFC 9421 style) or agent token, "
     "per-agent spend caps, repayment receipts + audit log. Mirrors the AP2/UCP trust model.",
     "2026-11-20T12:00:00Z"),
    ("v0.6.0 - MCP Surface & End-to-End Demo",
     "Expose catalog + checkout as MCP tools so LLM agents can drive a purchase; an example agent "
     "client; a full demo merchant with a walkthrough. The hackathon showpiece.",
     "2026-12-10T12:00:00Z"),
    ("v1.0.0 - Hardening & Production Readiness",
     "Persistence, observability, external config, rate limiting, security pass, CLI. Makes the "
     "scaffold a shippable service.",
     "2027-02-15T12:00:00Z"),
]

# non-default labels to create
LABELS = {
    "aiter-core": "aiter-core crate (schemas/protocol logic)",
    "aiter-server": "aiter-server crate (HTTP surface)",
    "payments": "payment rails: Razorpay, x402, UPI",
    "agent-auth": "agent identity, signing, spend caps",
    "mcp": "MCP / LLM surface",
    "demo": "demo merchant, seed data, walkthroughs",
    "security": "security & abuse protection",
    "observability": "tracing, metrics, logs",
    "discovery": "catalog discovery, well-known profiles",
    "testing": "unit/integration/property tests",
    "architecture": "design, storage, config, tooling",
    "good-first": "easy entry point for new contributors",
    "tooling": "CLI and dev tooling",
}

# ----- (milestone_title, title, body, [labels]) -----
ISSUES = [
    # ---- v0.1.0 Core Data Model ----
    ("v0.1.0 - Core Data Model & Foundations",
     "Amount: money as integer minor units + ISO 4217 currency",
     "Add an `Amount` type in `aiter-core`: integer minor units plus an ISO 4217 currency code, "
     "mirroring UCP's 'no floats on the wire' rule for money-adjacent math.\n\n"
     "Acceptance:\n- `Amount { units: i64, currency: Currency }` with ops for add/sub/mul-by-"
     "quantity, no floating point.\n- `Currency` enum or validated code + minor-unit exponent.\n"
     "- Unit tests for overflow guards and cross-currency refusal.\n\n"
     "Crate: `aiter-core`, zero dependencies.",
     ["enhancement", "good first issue", "aiter-core"]),
    ("v0.1.0 - Core Data Model & Foundations",
     "Product / catalog item model",
     "Define `Product` (id, title, price Amount, description, tags, image url, available qty) and the "
     "merchant catalog type in `aiter-core`.\n\n"
     "Acceptance:\n- Serde (de)serialize round-trip.\n- Variant-aware (options/skus) optional "
     "structure.\n- Unit tests for validation (non-negative price, required id).\n\n"
     "Crate: `aiter-core`.",
     ["enhancement", "good first issue", "aiter-core"]),
    ("v0.1.0 - Core Data Model & Foundations",
     "Cart model",
     "Define `Cart` (line items of product_id + quantity, per-line totals, cart totals) in "
     "`aiter-core`.\n\n"
     "Acceptance:\n- Add/update/remove line item operations pure and tested.\n- Totals derived "
     "from line items.\n- Serde round-trip.\n\nCrate: `aiter-core`.",
     ["enhancement", "good first issue", "aiter-core"]),
    ("v0.1.0 - Core Data Model & Foundations",
     "CheckoutSession model",
     "Define `CheckoutSession` (id, cart snapshot, currency, fulfillment selection, status, "
     "expires_at, amounts) in `aiter-core`, aligned to ACP's checkoutsession shape.\n\n"
     "Acceptance:\n- Clear status enum (pending, ready, paid, cancelled, failed).\n- Serde round-"
     "trip.\n- Unit tests for transitions.\n\nCrate: `aiter-core`.",
     ["enhancement", "aiter-core"]),
    ("v0.1.0 - Core Data Model & Foundations",
     "Order model + status enum",
     "Define `Order` (id, checkout_session_id, totals, status, timeline) in `aiter-core`.\n\n"
     "Acceptance:\n- Order status enum + allowed transitions.\n- Serde round-trip.\n- Unit tests.\n\n"
     "Crate: `aiter-core`.",
     ["enhancement", "aiter-core"]),
    ("v0.1.0 - Core Data Model & Foundations",
     "Merchant identity + agent role types",
     "Define merchant identity (`MerchantProfile`: id, name, pay-to destination, public key/url) "
     "and the actor roles (Merchant, Agent, Processor) used across the codebase.\n\n"
     "Acceptance:\n- Types compile and serialize.\n- Role model documented.\n\nCrate: `aiter-core`.",
     ["enhancement", "architecture", "aiter-core"]),
    # ---- v0.2.0 Catalog & Discovery ----
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "Catalog feed REST endpoint",
     "Expose the catalog to agents: `GET /catalog/products` with pagination + filter params on "
     "`aiter-server`, served from the core catalog store.\n\n"
     "Acceptance:\n- JSON response of Product list, stable pagination.\n- Integration test hits the "
     "endpoint and parses products.\n- JSON schema documented.\n\nCrates: `aiter-server`, `aiter-core`.",
     ["enhancement", "aiter-server", "discovery"]),
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "Product lookup + search",
     "Add `GET /catalog/products/{id}` and a keyword `search` query to `aiter-server` so an agent "
     "can resolve products.\n\n"
     "Acceptance:\n- Lookup returns 404 for unknown id.\n- Search ranks by title match.\n- Tests.\n\n",
     ["enhancement", "aiter-server", "discovery", "testing"]),
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "Merchant discovery profile: /.well-known/agent-card.json",
     "Serve a machine-readable discovery profile at `/.well-known/agent-card.json` (A2A-style "
     "agent card) advertising the merchant name, endpoint, and capabilities.\n\n"
     "Acceptance:\n- Endpoint serves valid JSON.\n- Capabilities listed match implemented endpoints.\n"
     "- Test asserts the well-known path.\n\n",
     ["enhancement", "aiter-server", "discovery"]),
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "llms.txt / agent-readable catalog export",
     "Emit a plain-text, LLM-readable catalog (`/llms.txt` and/or a markdown dump) so agents can "
     "grok the catalog before calling APIs.\n\n"
     "Acceptance:\n- Deterministic output from the store.\n- Documented format.\n\n",
     ["documentation", "discovery"]),
    ("v0.2.0 - Agent-Facing Catalog & Discovery",
     "Demo merchant seed catalog + tests",
     "Add a seed catalog (a small demo merchant, e.g. a coffee shop) with fixtures used by tests "
     "and the demo.\n\n"
     "Acceptance:\n- Seeds load from JSON and expose at least ~8 products.\n- Tests use fixtures not "
     "random data.\n\n",
     ["demo", "testing"]),
    # ---- v0.3.0 Cart & Checkout Flow ----
    ("v0.3.0 - Cart & Checkout Flow",
     "Cart API (create / update / cancel)",
     "Add REST endpoints on `aiter-server`: `POST /carts`, `GET|PUT /carts/{id}`, "
     "`POST /carts/{id}/cancel`.\n\n"
     "Acceptance:\n- Create returns a cart id.\n- Update re-derives totals.\n- Cancel idempotent.\n"
     "- Integration tests.\n\n",
     ["enhancement", "aiter-server", "aiter-core"]),
    ("v0.3.0 - Cart & Checkout Flow",
     "Checkout session API (create / complete / cancel)",
     "Add `POST /checkout_sessions`, `POST /checkout_sessions/{id}/complete`, and "
     "`POST /checkout_sessions/{id}/cancel` on `aiter-server`, aligned to the ACP checkout shape.\n\n"
     "Acceptance:\n- Creating a session snapshots the cart.\n- Complete finalizes totals + produces "
     "an Order.\n- Idempotent cancel.\n- Tests.\n\n",
     ["enhancement", "aiter-server", "aiter-core"]),
    ("v0.3.0 - Cart & Checkout Flow",
     "Totals computation (line totals, subtotal, taxes)",
     "Implement pricing in `aiter-core`: per-line totals from quantity x price, subtotal, and a "
     "pluggable tax hook.\n\n"
     "Acceptance:\n- Exact integer math, no float.\n- Configurable tax function.\n- Property tests\n"
     "over random carts.\n\n",
     ["enhancement", "aiter-core", "testing"]),
    ("v0.3.0 - Cart & Checkout Flow",
     "Checkout state machine + idempotency",
     "Make checkout transitions a tested state machine with idempotent retries (same event twice = "
     "no double effect).\n\n"
     "Acceptance:\n- Illegal transitions rejected.\n- Idempotency keys honored.\n- Unit tests.\n\n",
     ["enhancement", "aiter-core", "architecture"]),
    ("v0.3.0 - Cart & Checkout Flow",
     "In-memory store + storage trait",
     "Define a `Store` trait (create/get/update) and an in-memory `HashMap` implementation, so "
     "persistence can swap in later.\n\n"
     "Acceptance:\n- Trait is small and implementable.\n- In-memory impl correctness tested.\n\n",
     ["enhancement", "architecture", "aiter-core"]),
    # ---- v0.4.0 Razorpay Rail ----
    ("v0.4.0 - Payments Rail: Razorpay",
     "Razorpay API client (signing + Orders API)",
     "Add a Rust Razorpay client in `aiter-core`/`aiter-server`: Basic auth with Key:Secret, "
     "`POST /v1/orders` to create an order.\n\n"
     "Acceptance:\n- Credentials from env, never logged.\n- Order creation returns order_id.\n"
     "- Sandbox base URL default.\n\nNote: standard Razorpay Orders API per razorpay.com/docs.",
     ["enhancement", "payments"]),
    ("v0.4.0 - Payments Rail: Razorpay",
     "Payment-link generation + checkout redirect",
     "Given an order, generate a Razorpay payment link (`POST /v1/payment_links`) and return the "
     "`short_url` the buyer/client opens.\n\n"
     "Acceptance:\n- Link generated for a completed checkout.\n- Payload includes correct amount/"
     "currency/customer.\n- Tested against sandbox where possible.\n\n",
     ["enhancement", "payments", "aiter-server"]),
    ("v0.4.0 - Payments Rail: Razorpay",
     "Webhook HMAC signature verification",
     "Verify Razorpay webhooks with the HMAC-SHA256 signature before processing; expose `POST "
     "/webhooks/razorpay`.\n\n"
     "Acceptance:\n- Valid signature accepted, invalid rejected.\n- Constant-time comparison.\n"
     "- Unit tests with a known signature fixture.\n\n",
     ["enhancement", "security", "payments"]),
    ("v0.4.0 - Payments Rail: Razorpay",
     "Order-paid reconciliation",
     "On a verified `payment.paid` webhook, mark the Order paid and store the transaction id/"
     "receipt.\n\n"
     "Acceptance:\n- Idempotent (duplicate webhook no-op).\n- Order status transitions to paid.\n"
     "- Reconciliation is testable without real Razorpay.\n\n",
     ["enhancement", "payments", "aiter-core"]),
    ("v0.4.0 - Payments Rail: Razorpay",
     "UPI Reserve Pay: consent + agent-debit flow",
     "Integrate the agentic showpiece: user gives one-time consent + spending limit (UPI Reserve "
     "Pay / NPCI SBMD), then the agent can debit within limits without re-auth.\n\n"
     "Acceptance:\n- Consent capture endpoint.\n- Limit enforcement before debit.\n- Mobile/PC "
     "mismatch handled.\n- Flag as gated on Razorpay early-access availability.\n\n",
     ["enhancement", "payments", "agent-auth"]),
    ("v0.4.0 - Payments Rail: Razorpay",
     "Sandbox + external config plumbing",
     "Route Razorpay keys, base URLs, and secrets through env/config (`.env.example`), with a "
     "clearly separated sandbox vs live mode.\n\n"
     "Acceptance:\n- `.env.example` documents all vars.\n- No secret in git.\n- Mode is explicit.\n\n",
     ["architecture", "security"]),
    # ---- v0.5.0 Agent Identity & Trust ----
    ("v0.5.0 - Agent Identity & Trust",
     "Agent identity + request signing (RFC 9421 style)",
     "Model an agent identity and sign requests (HTTP message signatures per RFC 9421) so the "
     "merchant can authenticate an agent and prove intent.\n\n"
     "Acceptance:\n- Sign/verify round-trip with Ed25519 or ECDSA.\n- Tampered request rejected.\n"
     "- Mirrors the UCP/AP2 trust model.\n\n",
     ["enhancement", "agent-auth", "aiter-core"]),
    ("v0.5.0 - Agent Identity & Trust",
     "Request verification middleware",
     "A middleware on `aiter-server` that verifies an agent's signature/token on wrote endpoints "
     "before processing.\n\n"
     "Acceptance:\n- Unauthorized requests rejected with 401/403.\n- Well-known/info endpoints stay "
     "public.\n- Tests.\n\n",
     ["enhancement", "agent-auth", "aiter-server", "security"]),
    ("v0.5.0 - Agent Identity & Trust",
     "Per-agent spend limits + enforcement",
     "Attach spend caps to an agent identity and enforce them at checkout time.\n\n"
     "Acceptance:\n- Over-limit checkout rejected with a clear error.\n- Caps configurable per agent.\n"
     "- Tests.\n\n",
     ["enhancement", "agent-auth"]),
    ("v0.5.0 - Agent Identity & Trust",
     "Receipts + audit log",
     "Emit a repayment receipt per order and append to an audit log (who, what, when, amount) for "
     "accountability.\n\n"
     "Acceptance:\n- Receipt struct in core.\n- Audit log entries are append-only.\n- Tests.\n\n",
     ["enhancement", "agent-auth", "observability"]),
    # ---- v0.6.0 MCP Surface & Demo ----
    ("v0.6.0 - MCP Surface & End-to-End Demo",
     "MCP server binding (catalog + checkout tools)",
     "Expose catalog lookup and checkout as Model Context Protocol tools so ChatGPT/Claude/Gemini "
     "agents can drive a purchase.\n\n"
     "Acceptance:\n- Tools: list products, get product, create cart, complete checkout.\n- Runs "
     "against the same core.\n- Tested via an MCP client.\n\n",
     ["enhancement", "mcp", "aiter-server"]),
    ("v0.6.0 - MCP Surface & End-to-End Demo",
     "Example agent client (aiter-cli or MCP-driven)",
     "Ship a small example agent that talks to an AITER merchant: discover catalog, build a cart, "
     "check out, and report the payment link.\n\n"
     "Acceptance:\n- One-command run against a local server.\n- Prints a real payment link.\n\n",
     ["enhancement", "mcp", "demo"]),
    ("v0.6.0 - MCP Surface & End-to-End Demo",
     "End-to-end integration test (agent buys, order paid)",
     "A full integration test: agent lists catalog, adds to cart, completes checkout, generates "
     "payment, receives a verified webhook, order becomes paid.\n\n"
     "Acceptance:\n- Self-contained (sandbox + mocked Razorpay where needed).\n- Runs in CI.\n\n",
     ["testing", "demo"]),
    ("v0.6.0 - MCP Surface & End-to-End Demo",
     "Demo merchant + README walkthrough",
     "A repeatable demo: seed merchant, local run instructions, expected output, and a short "
     "walkthrough in the README.\n\n"
     "Acceptance:\n- `cargo run -p aiter-server` + seed = working demo.\n- README has concrete steps.\n"
     "- Screenshots/output samples.\n\n",
     ["documentation", "demo"]),
    # ---- v1.0.0 Hardening ----
    ("v1.0.0 - Hardening & Production Readiness",
     "Persistent store (sqlite or sled)",
     "Replace/extend the in-memory store with a durable backend so carts/orders survive restarts.\n\n"
     "Acceptance:\n- Store trait unchanged for callers.\n- Data survives restart.\n- Tests.\n\n",
     ["enhancement", "architecture"]),
    ("v1.0.0 - Hardening & Production Readiness",
     "Tracing spans + metrics",
     "Add structured tracing across handlers and basic request/order metrics for observability.\n\n"
     "Acceptance:\n- Spans on key operations.\n- Metrics endpoint or exported counters.\n\n",
     ["enhancement", "observability"]),
    ("v1.0.0 - Hardening & Production Readiness",
     "External config + CLI",
     "Consolidate config into a file/env loader and add a small CLI (`aiter-server run|init|seed`).\n\n"
     "Acceptance:\n- Config precedence documented.\n- CLI works.\n\n",
     ["architecture", "tooling"]),
    ("v1.0.0 - Hardening & Production Readiness",
     "Rate limiting + abuse protection",
     "Add rate limiting on wrote endpoints and basic abuse protection (per-identity quotas).\n\n"
     "Acceptance:\n- Limits configurable.\n- Overflow returns 429.\n- Tests.\n\n",
     ["security", "architecture"]),
    ("v1.0.0 - Hardening & Production Readiness",
     "Security pass: input validation + error handling",
     "Audit input handling, error responses, and panics on all surfaces; ensure no secret leakage in "
     "logs or errors.\n\n"
     "Acceptance:\n- Malformed input returns clean 4xx.\n- No secrets in logs.\n- Fuzz-ish property "
     "tests on parsers.\n\n",
     ["security", "testing"]),
]

def main():
    created = {"milestones": 0, "labels": 0, "issues": 0}

    # Milestones
    num_by_title = {}
    for title, desc, due in MILESTONES:
        out = gh("--method", "POST", f"repos/{REPO}/milestones",
                 "-f", f"title={title}", "-f", f"description={desc}", "-f", f"due_on={due}")
        if not out:
            print("SKIPPED milestone (may exist):", title)
            # try to fetch existing
            e = gh(f"repos/{REPO}/milestones?state=all")
            if e:
                for m in json.loads(e):
                    if m["title"] == title:
                        num_by_title[title] = m["number"]
            continue
        j = json.loads(out)
        num_by_title[title] = j["number"]
        created["milestones"] += 1
        print("milestone:", title, "-> #", j["number"])

    # Labels
    existing = set()
    e = gh(f"repos/{REPO}/labels?per_page=100")
    if e:
        existing = {x["name"] for x in json.loads(e)}
    for name, desc in LABELS.items():
        if name in existing:
            continue
        ok, msg = gh_label(name, "--description", desc)
        if ok:
            created["labels"] += 1
            print("label:", name)
        else:
            print("label skip:", name, msg)

    # Issues: create without --milestone (gh bug: --milestone fails to resolve),
    # then assign the milestone via gh api PATCH (reliable).
    title_to_milestone = {t: n for t, n in num_by_title.items()}
    for mt, title, body, labels in ISSUES:
        mn = title_to_milestone.get(mt)
        cmd = [GH, "issue", "create", "--repo", REPO, "--title", title, "--body", body,
               "--label", ",".join(labels)]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            print("FAILED issue:", title, "|", r.stderr.strip()[:200])
            continue
        created["issues"] += 1
        url = r.stdout.strip()
        num = url.rstrip("/").split("/")[-1]
        if mn:
            m = gh("--method", "PATCH", f"repos/{REPO}/issues/{num}", "-f", f"milestone={mn}")
            ms = ""
            if m:
                try:
                    ms = json.loads(m)["milestone"]["title"]
                except Exception:
                    ms = ""
            print(f"issue: #{num} [{mt[0:7]}] {title} -> {ms}")

    print("\n=== SUMMARY ===", json.dumps(created))
    print("Milestone map:", json.dumps(num_by_title))

if __name__ == "__main__":
    main()
