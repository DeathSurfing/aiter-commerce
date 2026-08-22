# AITER COMMERCE

Rust-first agentic commerce. Making a merchant **AI-buyable** — agents discover, shop, and complete purchases with any business, while the business keeps its existing checkout and payment rails.

Built for the agentic-payments era: an open-commerce layer that plugs into the emerging protocol stack (ACP / UCP / AP2 / x402 / UPI Reserve Pay) without forcing a merchant to rebuild anything.

## Why Rust

Low overhead, single static binaries, strong typing for money-adjacent correctness, and no runtime or toolchain bloat. Core commerce/schema logic lives in pure Rust; the HTTP surface is a thin layer on top.

## Workspace layout

```
crates/
├── aiter-core/    # library — schemas, protocol primitives, merchant-side logic (pure, minimal deps)
└── aiter-server/  # thin HTTP server — axum, exposes the agent-facing + merchant-facing surface
```

`aiter-server` ships three binaries:

- `aiter-server` — the HTTP router with a tiny `run | init | seed` CLI (`cargo run -p aiter-server`; see [Configuration](#configuration-issue-34) and the [CLI](#cli) table)
- `aiter-cli` — an example agent client that drives the signed flow end to end
- `mcp` — an MCP stdio server exposing the same state as Model Context Protocol tools (`cargo run -p aiter-server --bin mcp`)

## Status

The Day-1/Day-2 demo surface is feature-complete on `main`: agent-readable catalog + discovery, cart/checkout flow, Razorpay payment links + webhook reconciliation, request signing/trust, spend caps, receipts, and an MCP binding. Work is tracked in [GitHub milestones](https://github.com/DeathSurfing/aiter-commerce/milestones). Everything below is a single `cargo run -p aiter-server` away.

## Quickstart / Demo

A repeatable demo merchant: run the server, then drive the public discovery surface with `curl`, and (optionally) the signed agent flow end to end.

### 1. Environment (optional — the server runs with zero config)

Nothing is required to boot the server. Every variable in [`.env.example`](.env.example) is optional; `RAZORPAY_MODE` defaults to `sandbox` and `PORT` defaults to `8080`. The Razorpay keys are only consulted at the moment a payment link is minted or a webhook arrives (sandbox defaults protect you from accidental live charges; see [Payments](#payments-razorpay) below).

To set values explicitly:

```bash
cp .env.example .env        # then edit .env
set -a && source .env && set +a
```

or export the variables you need directly, e.g. `export PORT=9000`. Since #34, `aiter-server` also reads a `KEY=VALUE` config file directly — `./aiter.env` by default, `AITER_CONFIG=<path>` to override; see [Configuration](#configuration-issue-34).

### 2. Run the server

```bash
cargo run -p aiter-server
```

Expected output:

```
   Compiling aiter-server v0.1.0 ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.5s
     Running `target/debug/aiter-server`
2026-08-22T09:00:00.123456Z  INFO aiter_server: aiter-server listening on 0.0.0.0:8080
```

The server is now on **http://localhost:8080**. Seeding is automatic and deterministic: the merchant catalog is embedded in the binary at compile time (from `crates/aiter-server/fixtures/catalog.json` and the in-code seed catalog) — no database, no migrations.

### 3. Public flow — discovery + catalog (unauthenticated, `curl`)

Every endpoint below is public; no agent identity is needed to read.

```bash
# Service identity + liveness
curl -s localhost:8080/
curl -s localhost:8080/agentic/health

# The merchant's catalog (paginated envelope; id-ascending order)
curl -s localhost:8080/catalog/products
curl -s "localhost:8080/catalog/products?tag=hot"
curl -s "localhost:8080/catalog/products?search=latte"
curl -s localhost:8080/catalog/products/p-latte

# Discovery + LLM-readable export
curl -s localhost:8080/.well-known/agent-card.json
curl -s localhost:8080/llms.txt

# The embedded demo seed (a second, INR-priced catalog)
curl -s localhost:8080/seed/catalog
```

What you get back:

`GET /`:

```json
{
  "name": "AITER COMMERCE",
  "version": "0.1.0",
  "repo": "https://github.com/DeathSurfing/aiter-commerce",
  "agentic": true,
  "status": "ok"
}
```

`GET /agentic/health`:

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

`GET /catalog/products` — paginated envelope `{ items, total, limit, offset, has_more }`; `limit` defaults to 25 (cap 100), `offset` pages, `?tag=` filters case-insensitively, `?search=` re-ranks by relevance (title > tag > description, then stable id order). Money is always integer minor units — `450` USD = $4.50:

```json
{
  "items": [
    {
      "id": "p-coldbrew",
      "title": "Cold Brew",
      "price": { "units": 500, "currency": "USD" },
      "description": "Smooth, slow-steeped cold brew coffee.",
      "tags": ["cold", "coffee"],
      "available_qty": 8
    },
    {
      "id": "p-espresso",
      "title": "Espresso",
      "price": { "units": 300, "currency": "USD" },
      "description": "A single shot of rich espresso.",
      "tags": ["hot", "coffee"],
      "available_qty": 20
    },
    {
      "id": "p-latte",
      "title": "Caffè Latte",
      "price": { "units": 450, "currency": "USD" },
      "description": "Espresso with steamed milk.",
      "tags": ["hot", "coffee"],
      "available_qty": 10
    }
  ],
  "total": 3,
  "limit": 25,
  "offset": 0,
  "has_more": false
}
```

`GET /catalog/products?search=latte` — title matches rank first, so only the latte survives:

```json
{
  "items": [
    {
      "id": "p-latte",
      "title": "Caffè Latte",
      "price": { "units": 450, "currency": "USD" },
      "description": "Espresso with steamed milk.",
      "tags": ["hot", "coffee"],
      "available_qty": 10
    }
  ],
  "total": 1,
  "limit": 25,
  "offset": 0,
  "has_more": false
}
```

`GET /catalog/products/p-latte` — a single product (same shape; `image_url` and `variant` are omitted when absent, `404 {"error":"product not found"}` for unknown ids):

```json
{
  "id": "p-latte",
  "title": "Caffè Latte",
  "price": { "units": 450, "currency": "USD" },
  "description": "Espresso with steamed milk.",
  "tags": ["hot", "coffee"],
  "available_qty": 10
}
```

`GET /.well-known/agent-card.json` — A2A-style discovery card with **absolute** endpoint URLs resolved from the request host:

```json
{
  "agent": { "name": "AITER COMMERCE", "version": "0.1.0" },
  "url": "https://github.com/DeathSurfing/aiter-commerce",
  "service": "http://localhost:8080",
  "capabilities": [
    "catalog", "search", "discovery", "llms",
    "carts", "checkout_sessions", "seed", "health"
  ],
  "endpoints": {
    "catalog": "http://localhost:8080/catalog/products",
    "product_lookup": "http://localhost:8080/catalog/products/{id}",
    "search": "http://localhost:8080/catalog/products?search={query}",
    "discovery": "http://localhost:8080/.well-known/agent-card.json",
    "llms": "http://localhost:8080/llms.txt",
    "carts": "http://localhost:8080/carts",
    "checkout_sessions": "http://localhost:8080/checkout_sessions",
    "seed": "http://localhost:8080/seed/catalog",
    "health": "http://localhost:8080/agentic/health"
  }
}
```

`GET /llms.txt` — deterministic llms.txt-shaped export, parsed by generic llms.txt clients:

```text
# AITER COMMERCE catalog

> Machine-readable catalog of products available from this merchant.
> Served in stable (id-ascending) order.
> - [Browse catalog](/catalog/products)
> - [Agent card](/.well-known/agent-card.json)

## Products

- [Cold Brew](/catalog/products/p-coldbrew): Smooth, slow-steeped cold brew coffee.
- [Espresso](/catalog/products/p-espresso): A single shot of rich espresso.
- [Caffè Latte](/catalog/products/p-latte): Espresso with steamed milk.
```

`GET /seed/catalog` — the embedded demo seed: a 10-product, INR-priced coffee shop (Caffè Latte at `{ "units": 35000, "currency": "INR" }` = ₹350.00, etc.) exported as a `Catalog`:

```json
{
  "products": [
    {
      "id": "espresso",
      "title": "Espresso",
      "price": { "units": 25000, "currency": "INR" },
      "description": "Double shot of our house espresso blend.",
      "tags": ["coffee", "hot"],
      "available_qty": 100
    }
  ]
}
```

(10 products total: espresso, americano, cappuccino, latte, flat-white, caffe-mocha, cold-brew, filter-coffee, croissant, blueberry-muffin.)

### 4. Signed agent flow (cart, checkout, order, payment link, webhook)

**Writes require an agent signature.** Every mutating endpoint (`POST /carts`, `PUT /carts/{id}`, `POST /carts/{id}/cancel`, `POST /checkout_sessions`, `POST /checkout_sessions/{id}/complete`, `POST /checkout_sessions/{id}/cancel`, `POST /orders/{id}/payment_link`) is guarded by the `require_signed` middleware: the request must carry

- `x-agent-id` — the agent's id, and
- `x-request-signature` — a JSON-serialized RFC 9421-style `RequestSignature` envelope covering method, target URI, body digest (`sha-256=:<base64>:`), timestamp, and agent id, signed with the agent's Ed25519 key.

Missing/malformed/invalid signatures get **401**; a validly-signed request from an **unregistered** agent gets **403**. `GET /carts/{id}` and `POST /webhooks/razorpay` are the deliberate public exceptions (cart reads mutate nothing; Razorpay authenticates webhooks with its own HMAC).

**There is no pre-registered demo agent on the server.** The HTTP server boots with an empty agent registry (`AppState::new(...)` starts with no agents — writes would get `403 unknown agent <id>`), so an agent must be admitted out of band, exactly as the UCP/AP2 trust model expects: the merchant knows the agent's `AgentIdentity` (id + Ed25519 public key) and registers it with a per-agent spend cap via `AppState::register_agent`. The complete signing pattern lives in `crates/aiter-server/tests/trust.rs` (and the checkout tests) — this is the canonical example:

```rust
use aiter_core::amount::{Amount, Currency};
use aiter_core::signing::AgentKeypair;
use aiter_server::catalog::{seed_catalog, AppState};

#[tokio::main]
async fn main() {
    let state = AppState::new(seed_catalog());

    // Admit a demo agent with a $10,000 USD spend cap (cap is in minor units).
    let keypair = AgentKeypair::generate();
    let identity = keypair.identity("agent-1".to_string());
    state.register_agent(identity.clone(), Amount::new(1_000_000, Currency::USD)).await;

    let app = aiter_server::router(state);

    // Sign every mutating request:
    let signature = keypair.sign_request(&identity.id, "POST", "/carts",
        br#"{"currency":"USD","items":[{"product_id":"p-latte","quantity":2}]}"#,
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    // -> send headers:
    //    x-agent-id: agent-1
    //    x-request-signature: {"agent_id":"agent-1","method":"POST","target_uri":"/carts",
    //      "content_digest":"sha-256=:<base64 sha-256 of body>:","timestamp":<unix seconds>,
    //      "signature":"<base64 ed25519>"}
}
```

The HTTP request a signed agent sends looks like this (`x-request-signature` values below are illustrative):

```bash
curl -s -X POST localhost:8080/carts \
  -H "content-type: application/json" \
  -H "x-agent-id: agent-1" \
  -H 'x-request-signature: {"agent_id":"agent-1","method":"POST","target_uri":"/carts","content_digest":"sha-256=:tQkK7F...:","timestamp":1755500000,"signature":"QUlURVItREVNTy1TSUdOQVRVUkUtMDAx"}' \
  -d '{"currency":"USD","items":[{"product_id":"p-latte","quantity":2}]}'
```

**Cart → checkout session → order → payment link → webhook**, all signed except the webhook:

```bash
# 1. Create a cart (2 × p-latte = 900 minor units = $9.00)
SIG=$(# request-signature envelope as above, method=POST, target_uri=/carts)
curl -s -X POST localhost:8080/carts \
  -H "content-type: application/json" -H "x-agent-id: agent-1" -H "x-request-signature: $SIG" \
  -d '{"currency":"USD","items":[{"product_id":"p-latte","quantity":2}]}'
```

```json
{
  "id": "cart-0",
  "cancelled": false,
  "cart": {
    "currency": "USD",
    "items": [{ "product_id": "p-latte", "quantity": 2 }]
  },
  "totals": {
    "subtotal": { "units": 900, "currency": "USD" },
    "tax": { "units": 0, "currency": "USD" },
    "total": { "units": 900, "currency": "USD" }
  }
}
```

```bash
# 2. Read the cart back (public — reads never mutate state)
curl -s localhost:8080/carts/cart-0

# 3. Snapshot the cart into a checkout session (signed)
curl -s -X POST localhost:8080/checkout_sessions \
  -H "content-type: application/json" -H "x-agent-id: agent-1" -H "x-request-signature: $SIG" \
  -d '{"cart_id":"cart-0"}'
```

```json
{
  "id": "cs-0",
  "cart": {
    "currency": "USD",
    "items": [{ "product_id": "p-latte", "quantity": 2 }]
  },
  "currency": "USD",
  "fulfillment": "Pickup",
  "status": "Pending",
  "expires_at": 1755503600,
  "totals": {
    "subtotal": { "units": 900, "currency": "USD" },
    "tax": { "units": 0, "currency": "USD" },
    "total": { "units": 900, "currency": "USD" }
  }
}
```

```bash
# 4. Complete the checkout (signed): Pending -> Ready -> Paid, an Order is created,
#    the agent is charged against its spend cap, and a receipt is appended to the
#    audit log (exactly once; re-completing is an idempotent no-op).
curl -s -X POST localhost:8080/checkout_sessions/cs-0/complete \
  -H "x-agent-id: agent-1" -H "x-request-signature: $SIG"
```

```json
{
  "id": "ord-cs-0",
  "checkout_session_id": "cs-0",
  "totals": {
    "subtotal": { "units": 900, "currency": "USD" },
    "tax": { "units": 0, "currency": "USD" },
    "total": { "units": 900, "currency": "USD" }
  },
  "status": "Placed",
  "timeline": [{ "to": "Placed", "at": 1755503600 }]
}
```

```bash
# 5. Mint a Razorpay payment link for the order (signed; requires Razorpay keys —
#    see "Payments" below). Without RAZORPAY_KEY_ID/RAZORPAY_KEY_SECRET this
#    returns 503 with a config error naming the missing variable.
curl -s -X POST localhost:8080/orders/ord-cs-0/payment_link \
  -H "x-agent-id: agent-1" -H "x-request-signature: $SIG"
```

```json
{
  "order_id": "ord-cs-0",
  "short_url": "https://rzp.io/l/aiter-demo-9f3k"
}
```

```bash
# 6. Razorpay delivers a payment.paid webhook (PUBLIC — verified by HMAC-SHA256
#    over the raw body with RAZORPAY_WEBHOOK_SECRET, header x-razorpay-signature).
#    The order is reconciled to its paid state and records the payment id;
#    duplicate deliveries are idempotent no-ops.
curl -s -X POST localhost:8080/webhooks/razorpay \
  -H "x-razorpay-signature: <hex hmac-sha256 of the raw body>" \
  -H "content-type: application/json" \
  -d '{"event":"payment.paid","payload":{"payment":{"entity":{"id":"pay_9f3k2a","notes":{"order_id":"ord-cs-0"}}}}}'
```

```json
{
  "received": true,
  "payment_id": "pay_9f3k2a",
  "status": "paid"
}
```

Behavioral notes on the signed flow:

- **Spend caps** (per agent, set at registration, in minor units): an over-limit checkout completes with **403** `spend limit exceeded for agent ...` and leaves the session untouched.
- **Receipts/audit log**: each completed agent-signed checkout appends exactly one `Receipt` (who = agent id, what = order id, when = unix seconds, amount = order total) to an append-only audit log — never mutated, never removed.
- **Idempotency**: re-completing a session returns the same order with no double charge and no second audit entry; cancels are idempotent; the webhook is idempotent.

An alternative to hand-signing: the MCP stdio server (`cargo run -p aiter-server --bin mcp`) drives the same handlers directly, so an MCP-enabled agent can `create_cart` → `complete_checkout` without HTTP signatures. Note that MCP-driven completions carry no agent identity, so spend-cap charging and receipts apply only to signed HTTP completions.

### Payments (Razorpay)

| Variable | Meaning | Default |
|---|---|---|
| `RAZORPAY_KEY_ID` | API key id from the Razorpay Dashboard (sandbox keys start `rzp_test_`) | — (required only to mint payment links / verify webhooks) |
| `RAZORPAY_KEY_SECRET` | API key secret | — (same) |
| `RAZORPAY_MODE` | `sandbox` or `live`; any other value is a config error at payment time | `sandbox` |
| `RAZORPAY_BASE_URL` | API base override (never in production) | `https://api.razorpay.com` |
| `RAZORPAY_WEBHOOK_SECRET` | Secret for HMAC-SHA256 webhook verification | — (webhooks **fail closed** without it) |
| `PORT` | HTTP listen port | `8080` |

Details:

- Keys are read **lazily, per request**, when a payment link is minted or a webhook arrives — the server boots fine without them (`503 {"error":"razorpay config error: RAZORPAY_KEY_ID is required"}` if you mint a link without keys).
- Credentials are **never logged**: every `Debug` impl and error path redacts the secret.
- `POST /webhooks/razorpay` is public by design (Razorpay signs deliveries with its own HMAC-SHA256, `x-razorpay-signature`, over the **raw** body — an agent signature cannot produce that). Without `RAZORPAY_WEBHOOK_SECRET` every webhook is refused (**401**, fail closed). `payment.paid` events drive the referenced order (`notes.order_id`) to its paid state and record the payment id; other events are acknowledged and ignored.
- All money sent to Razorpay is in **minor units** (cents for USD, paise for INR) — `amount: 900` for a $9.00 order.

## Configuration (issue #34)

`aiter-server` reads its configuration from **three layers**, lowest to highest precedence:

| Layer | Source |
|---|---|
| 1. **Defaults** | compiled in — `PORT=8080`, `RAZORPAY_MODE=sandbox`, `RAZORPAY_BASE_URL=https://api.razorpay.com`, keys unset |
| 2. **Config file** | optional `KEY=VALUE` file — `./aiter.env` by default, overridable with the `AITER_CONFIG` env var |
| 3. **Process env** | the real environment — `PORT` and `RAZORPAY_*` vars |

**Precedence: defaults < config file < process env vars (env wins).** Each key resolves independently (`env var → config file → default`), so a `PORT` env var beats a file `PORT`, which beats the 8080 default — and the same per-key rule applies to every `RAZORPAY_*` variable. Values that appear nowhere fall back to their defaults (or stay unset: the Razorpay keys are only required lazily, when a payment link is minted or a webhook arrives).

Backwards compatible by construction: **when no config file exists, the server is env-only and behaves exactly as before #34** — `PORT` and `RAZORPAY_*` env vars keep working, including the old silent fallback to 8080 for an unparseable `PORT` env var.

### Config file format

- `KEY=VALUE` lines; `#` comments and blank lines are ignored; keys and values are trimmed.
- Empty values count as unset (a fresh `init` template behaves like a no-file run).
- Unknown keys are ignored (with a warning) so newer files stay readable by older binaries.
- A malformed line (no `=`) or an unparseable `PORT` in the file is a **startup error** — typos surface immediately.

Generate a commented template containing every current default:

```bash
aiter-server init                      # writes ./aiter.env (refuses to overwrite)
AITER_CONFIG=prod.env aiter-server init   # or write elsewhere
```

Then edit it and start the server — no shell source-ing needed:

```bash
aiter-server run                       # `run` is the default; bare `aiter-server` works too
AITER_CONFIG=prod.env aiter-server run
```

### CLI

The `aiter-server` binary is a dependency-free `run | init | seed` CLI:

| Command | Behavior |
|---|---|
| `aiter-server run` (default) | bind + serve exactly like before (config-file-aware port, `ConnectInfo`); bare `aiter-server` runs this too |
| `aiter-server init` | write a commented `KEY=VALUE` template with current defaults to the configured path; refuses to overwrite an existing file |
| `aiter-server seed` | print the embedded demo catalog fixture (`seed::demo_catalog`) — product ids + titles, no server |
| `aiter-server help` / `-h` | usage text |

## Agent catalog surface (`aiter-server`)

The server exposes an agent-readable catalog + discovery surface (Day 1):

| Endpoint | Purpose |
|---|---|
| `GET /catalog/products` | Paginated, filterable catalog feed (envelope with `items`/`total`/`has_more`, `?limit`/`?offset`/`?tag`/`?search`) |
| `GET /catalog/products/{id}` | Single product; `404` when unknown |
| `GET /.well-known/agent-card.json` | A2A-style merchant discovery card (capabilities/endpoints, absolute URLs) |
| `GET /llms.txt` | Deterministic, LLM-readable catalog export in the `llms.txt` convention |
| `GET /seed/catalog` | The embedded demo seed (10 INR products) |
| `GET /` , `GET /agentic/health` | Service identity + liveness |

Plus the signed write surface: `POST /carts`, `GET|PUT /carts/{id}`, `POST /carts/{id}/cancel`, `POST /checkout_sessions`, `POST /checkout_sessions/{id}/complete`, `POST /checkout_sessions/{id}/cancel`, `POST /orders/{id}/payment_link`, and the public `POST /webhooks/razorpay` (see the [walkthrough above](#4-signed-agent-flow-cart-checkout-order-payment-link-webhook)).

### `GET /catalog/products` schema

Returns a paginated **envelope** — `{ items, total, limit, offset, has_more }` —
so a client can walk every page and know when to stop. `items` is the array of
`aiter-core` `Product` objects (same shape as the core `Product`
serialization), ordered by `id` unless `?search=` re-ranks them:

```json
{
  "items": [
    {
      "id": "p-latte",
      "title": "Caffè Latte",
      "price": { "units": 450, "currency": "USD" },
      "description": "Espresso with steamed milk.",
      "tags": ["hot", "coffee"],
      "available_qty": 10
    }
  ],
  "total": 1,
  "limit": 25,
  "offset": 0,
  "has_more": false
}
```

`price.units` is an integer in the currency's minor units (never a float);
`currency` is an ISO 4217 code. `image_url` and `variant` are optional and
omitted when absent. `limit` defaults to `25` and is capped at `100`, so the
response stays bounded even when no `?limit=` is supplied.

Query parameters:

| Param | Meaning |
|---|---|
| `limit`, `offset` | Pagination over the stable id-ordered list |
| `tag` | Filter to products carrying this tag (case-insensitive) |
| `search` | Keyword search across title, tags and description; ranks title matches above tag-only above description-only |

### `GET /.well-known/agent-card.json`

The discovery card advertises `endpoints` as **absolute** URLs resolved from
the request `Host` (honouring `X-Forwarded-Proto`/`X-Forwarded-Host`), plus a
`service` base URL. A fresh agent can call the advertised endpoints without
external context.

### `GET /llms.txt`

Follows the de-facto [llms.txt](https://llmstxt.org) convention: a `#` title,
a `>` blockquote intro with links, then a `## Products` section of markdown
links (`- [Title](/catalog/products/{id}): description`) in stable id order.
Deterministic so a generic llms.txt client can parse it.

## Roadmap

AITER COMMERCE is built as a **weekend sprint**: everything demo-worthy lands within one weekend (target Sat 2026-08-22 – Sun 2026-08-23), with hardening pushed to a post-weekend backlog.

| Milestone | Focus | Window |
|---|---|---|
| [Day 1 · Foundation & Agent Catalog](https://github.com/DeathSurfing/aiter-commerce/milestone/8) | Core data model + agent-readable catalog/discovery | Sat AM |
| [Day 1 · Checkout Flow](https://github.com/DeathSurfing/aiter-commerce/milestone/9) | Cart + checkout sessions, totals, state machine | Sat PM |
| [Day 2 · Razorpay Payments Rail](https://github.com/DeathSurfing/aiter-commerce/milestone/10) | Orders, payment links, webhook, Reserve Pay | Sun AM |
| [Day 2 · Trust, MCP & End-to-End Demo](https://github.com/DeathSurfing/aiter-commerce/milestone/11) | Agent trust, MCP surface, e2e demo | Sun PM |
| [Backlog · Post-weekend hardening](https://github.com/DeathSurfing/aiter-commerce/milestone/12) | Persistence, observability, config, rate limits | later |

New contributors: start with a `good first issue` in the Day 1 milestone.

## Testing & CI

Every PR runs a mandatory quality pipeline before it can merge. Keeping this green is the baseline for shipping; broken code never lands on `main`.

Local (same gates CI runs):

```bash
./scripts/check.sh   # fmt --check, clippy -D warnings, test, build --release
```

CI (`.github/workflows/ci.yml`) runs the identical four gates on every push and pull request:

- `cargo fmt --check` — formatting
- `cargo clippy --all-targets --all-features -- -D warnings` — lint (warnings denied)
- `cargo test --all-features` — unit + integration tests
- `cargo build --release` — release build must compile

Run `./scripts/check.sh` before pushing. If CI fails on your PR, it fails for a real reason.

## License

MIT — see [LICENSE](LICENSE).

## Example agent client (`aiter-cli`)

### Demo agent 

The server pre-registers a well-known **demo agent** so example clients can
run against it with zero setup: id `agent-demo`, whose Ed25519 keypair is
derived from a **fixed, public seed** (`DEMO_AGENT_SEED` in
`crates/aiter-server/src/catalog.rs`). The demo key is deliberately **not a
secret** — anyone can reconstruct it from the source — so it is for demos and
tests only; production agents provision their own keys out of band.

### Example agent client (`aiter-cli`, issue #29)

With the server running (and `RAZORPAY_KEY_ID` / `RAZORPAY_KEY_SECRET`
sandbox keys set), one command buys a product and prints a real payment
link:

```bash
# first catalog product, qty 1
cargo run --bin aiter-cli -- --base http://localhost:8080

# a specific product and quantity
cargo run --bin aiter-cli -- --base http://localhost:8080 p-latte 2
```

The CLI discovers the catalog, builds a signed cart, creates and completes a
signed checkout session, mints a signed payment link, and prints its
`short_url` (plus the order id). Every write carries the demo agent's
signature (`x-agent-id` + `x-request-signature` headers), which
`AppState::default()` registers; a server that does not know the demo agent
rejects the writes with `401`/`403` and the CLI prints a hint pointing at the
registration.
