# AITER COMMERCE

AITER COMMERCE makes a merchant **agent-buyable**: it wraps an existing store and its payment rails in a machine-readable, agent-friendly API, so AI agents (LLMs and autonomous shoppers) can discover products, build a cart, and complete a purchase end to end — while the merchant keeps their existing storefront and checkout.

Concretely, an agent can:

1. **Discover the store** — browse the catalog feed, search it, and read an A2A-style discovery card (`/.well-known/agent-card.json`) or a plain-text `llms.txt` export. No API key, no auth.
2. **Buy with trust** — create a signed cart, snapshot it into a checkout session, and complete it into an order. Every mutating request carries the agent's RFC 9421-style Ed25519 signature, and the merchant enforces per-agent spend caps, receipts, and an append-only audit log.
3. **Pay through existing rails** — the order settles on the merchant's current payment provider (Razorpay today): a payment link is minted for the buyer and a verified webhook reconciles the order to paid. The merchant rebuilds nothing.

Built in Rust: `aiter-core` holds pure, money-safe commerce logic (integer minor units, never floats); `aiter-server` is a thin axum HTTP surface. Designed to plug into the emerging agentic-payments protocol stack (ACP / UCP / AP2 / x402 / UPI Reserve Pay) rather than invent its own.

## Workspace layout

```
crates/
├── aiter-core/    # schemas, protocol primitives, merchant-side logic (pure, minimal deps)
└── aiter-server/  # thin axum HTTP server — agent-facing + merchant-facing surface
```

`aiter-server` ships three binaries:

- `aiter-server` — the HTTP server, with a `run | init | seed` CLI
- `aiter-cli` — an example agent client that drives the signed buy flow end to end
- `mcp` — an MCP stdio server exposing the same state as Model Context Protocol tools

## Quickstart / Demo

### 1. Run the server

```bash
cargo run -p aiter-server
```

The server listens on **http://localhost:8080** with zero config: the demo catalog is embedded in the binary, and the `agent-demo` agent (a fixed, public Ed25519 seed) is pre-registered so the demo flow works out of the box.

### 2. Discovery — agents can read the store (curl)

All reads are public; no agent identity is needed.

```bash
curl -s localhost:8080/                                # service identity
curl -s localhost:8080/catalog/products                # catalog feed
curl -s "localhost:8080/catalog/products?search=latte" # search
curl -s localhost:8080/catalog/products/p-latte        # single product
curl -s localhost:8080/.well-known/agent-card.json     # A2A discovery card
curl -s localhost:8080/llms.txt                        # LLM-readable export
curl -s localhost:8080/seed/catalog                    # INR-priced demo seed
```

### 3. An agent buys

```bash
# buys the first catalog product (qty 1)
cargo run --bin aiter-cli -- --base http://localhost:8080

# buys a specific product and quantity
cargo run --bin aiter-cli -- --base http://localhost:8080 p-latte 2
```

The CLI is the "agent": it discovers the catalog, builds a **signed** cart, creates and completes a **signed** checkout session into an order, and mints a **signed** payment link, printing the buyer's `short_url`. Every write carries `x-agent-id` + `x-request-signature` headers.

Minting the payment link requires Razorpay keys (see [Payments](#payments-razorpay)). To demo the full flow offline, point `RAZORPAY_BASE_URL` at any mock that answers `POST /v1/payment_links`:

```bash
RAZORPAY_KEY_ID=test RAZORPAY_KEY_SECRET=test \
RAZORPAY_BASE_URL=http://127.0.0.1:9091 cargo run -p aiter-server
```

## Payments (Razorpay)

| Variable | Meaning | Default |
|---|---|---|
| `RAZORPAY_KEY_ID` | API key id (sandbox keys start `rzp_test_`) | — (required only to mint links / verify webhooks) |
| `RAZORPAY_KEY_SECRET` | API key secret | — (same) |
| `RAZORPAY_MODE` | `sandbox` or `live` | `sandbox` |
| `RAZORPAY_BASE_URL` | API base override (mock/gateway only) | `https://api.razorpay.com` |
| `RAZORPAY_WEBHOOK_SECRET` | HMAC-SHA256 webhook verification secret | — (webhooks fail closed without it) |
| `PORT` | HTTP listen port | `8080` |

- Keys are read **lazily, per request** — the server boots fine without them and errors (`503`) only when a link is minted or a webhook arrives.
- Credentials are **never logged**.
- `POST /webhooks/razorpay` is public by design: Razorpay signs deliveries with its own HMAC-SHA256 (`x-razorpay-signature`) over the raw body. Without `RAZORPAY_WEBHOOK_SECRET` every webhook is refused (fail closed). `payment.paid` events drive the referenced order (`notes.order_id`) to paid; duplicate deliveries are idempotent no-ops.
- All money is in **minor units** (cents for USD, paise for INR) — `amount: 900` = $9.00.

## Configuration

Precedence: **defaults < config file < process env (env wins)**; each key resolves independently.

- **Config file** — optional `KEY=VALUE` file at `./aiter.env` (override with `AITER_CONFIG=<path>`). `#` comments and blank lines are ignored; empty values count as unset; unknown keys are ignored; malformed lines and unparseable `PORT` values are startup errors.
- **CLI** — `aiter-server init` writes a commented template (refuses to overwrite), `aiter-server seed` prints the demo catalog, `aiter-server run` (the default) serves.

## HTTP surface

Public reads:

| Endpoint | Purpose |
|---|---|
| `GET /`, `GET /agentic/health` | Service identity + liveness |
| `GET /catalog/products` | Paginated, filterable catalog — `?limit`/`?offset`/`?tag`/`?search`, envelope `{ items, total, limit, offset, has_more }` |
| `GET /catalog/products/{id}` | Single product; `404` when unknown |
| `GET /.well-known/agent-card.json` | A2A-style discovery card (absolute endpoint URLs) |
| `GET /llms.txt` | Deterministic, LLM-readable catalog export |
| `GET /seed/catalog` | Embedded INR-priced demo seed (10 products) |

Signed writes (require `x-agent-id` + `x-request-signature`):

| Endpoint | Purpose |
|---|---|
| `POST /carts`, `GET\|PUT /carts/{id}`, `POST /carts/{id}/cancel` | Cart lifecycle |
| `POST /checkout_sessions` | Snapshot a cart into a checkout session |
| `POST /checkout_sessions/{id}/complete` | Pending → Ready → Paid; creates an Order |
| `POST /checkout_sessions/{id}/cancel` | Cancel a session (idempotent) |
| `POST /orders/{id}/payment_link` | Mint a Razorpay payment link |

Public exceptions: `GET /carts/{id}` (reads mutate nothing) and `POST /webhooks/razorpay` (authenticated by Razorpay's own HMAC).

### Trust model

Writes are guarded by the `require_signed` middleware. A request must carry:

- `x-agent-id` — the agent's id
- `x-request-signature` — an RFC 9421-style envelope covering method, target URI, body digest (`sha-256=:<base64>:`), timestamp, and agent id, signed with the agent's Ed25519 key.

Missing/malformed/invalid signatures → **401**; validly-signed requests from an **unregistered** agent → **403**. The canonical signing pattern is in `crates/aiter-server/tests/trust.rs`; the demo agent's keypair is derived from a fixed public seed (`DEMO_AGENT_SEED` in `crates/aiter-server/src/catalog.rs`) — demos and tests only.

Behavioral guarantees:

- **Spend caps** — per agent, set at registration (in minor units); an over-limit checkout is rejected with `403` and the session is left untouched.
- **Receipts / audit log** — each completed signed checkout appends exactly one receipt (who, what, when, amount) to an append-only log.
- **Idempotency** — re-completing a session, cancels, and duplicate webhooks are all no-ops.

## MCP

`cargo run -p aiter-server --bin mcp` exposes the same handlers as MCP stdio tools (`create_cart` → `complete_checkout`), so an MCP-enabled agent can drive the flow without HTTP signatures. Note that MCP-driven completions carry no agent identity, so spend-cap charging and receipts apply only to signed HTTP completions.

## Testing & CI

```bash
./scripts/check.sh   # cargo fmt --check, clippy -D warnings, test, build --release
```

CI (`.github/workflows/ci.yml`) runs the same four gates on every push and pull request.

## License

MIT — see [LICENSE](LICENSE).