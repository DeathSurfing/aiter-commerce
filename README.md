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
| `RUST_LOG` | tracing filter, e.g. `aiter_server=debug,tower_http=info` | `aiter_server=info,tower_http=info` |

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

CI (`.github/workflows/ci.yml`) runs the same four gates on every push and pull request. Two more workflows build on it: `docker.yml` is the Docker PR gate (builds the image and runs a compose smoke test on PRs), and `release.yml` builds and pushes multi-arch images to `ghcr.io/deathsurfing/aiter-commerce` on `v*` tags.

## Deploy with Docker

The server ships as a multi-stage, distroless image (`aiter-server` binary only, `gcr.io/distroless/cc-debian12`, non-root). A published image is built on tag push and served from **`ghcr.io/deathsurfing/aiter-commerce`** (see `release.yml`); `compose.yml` runs it by the local name `aiter-commerce` with a `build: .` fallback.

```bash
docker pull ghcr.io/deathsurfing/aiter-commerce:latest   # fetch the image
docker compose up -d                                     # start the server (builds from . if the image is missing)
cargo run --bin aiter-cli -- --base http://localhost:8080  # signed demo buy against the container
```

`compose.yml` maps the container's port 1:1 to the same `$PORT` (default `8080`) and reads all runtime config from `.env` (`env_file`) — copy `.env.example` to `.env` and fill in the Razorpay keys, exactly as in [Payments](#payments-razorpay). The demo buy mints a payment link, so it needs those keys (or a `RAZORPAY_BASE_URL` mock, see Quickstart). If you pulled the registry image instead of building, tag it `aiter-commerce` once so compose picks it up.

`compose.yml` ships with **no healthcheck**: the image is distroless (no shell, no `curl`), so an in-container `exec` probe is impossible. Probe liveness externally, or via a sidecar:

```bash
curl -sf localhost:8080/agentic/health   # 200 = up; GET / also works
```

Public reads are rate-limited per client IP: the TCP peer address, else the **first `x-forwarded-for` entry**, else `"local"` (sockless tests). Behind a reverse proxy the peer is the proxy, so **a trusted proxy you control must set `x-forwarded-for`** — otherwise every read throttles on the proxy's IP, and a caller can spoof the header. Terminate TLS at the proxy and forward to the container:

```caddy
# Caddyfile — Caddy sets X-Forwarded-For to the real client by default.
aiter.example.com {
    reverse_proxy localhost:8080
}
```

Keep the container's port private to the proxy; expose only the proxy to the internet. Traefik works the same way (its `forwardedHeaders` come from the trusted set).

**State is in-process today (demo-grade).** Carts, sessions, orders, and consents live in in-memory stores inside the server process, so a container restart loses them. When durable persistence lands (the `sled`-backed store in `aiter-core` is the plan), mount a volume for the store directory rather than redesigning the image.

## Deploy your own store

The binary ships with a small demo catalog baked into it (`crates/aiter-server/fixtures/catalog.json`). To serve **your** store you replace that fixture with your catalog and rebuild the image. The catalog is compiled in at build time (there is no runtime product-upload endpoint yet), so this is a build-time change.

### 1. Replace the catalog

Write your products into `crates/aiter-server/fixtures/catalog.json`. Each product has this shape; `price.units` is minor units (paise for INR, cents for USD), so ₹3,499 is `349900`:

```json
{
  "products": [
    {
      "id": "indigo-overshirt",
      "title": "Indigo Overshirt",
      "price": { "units": 349900, "currency": "INR" },
      "description": "Heavyweight cotton overshirt, garment dyed.",
      "tags": ["clothing", "winter"],
      "available_qty": 25
    }
  ]
}
```

- `id` must be unique: agents reference products by it (carts, sessions, orders).
- Keep **one `currency`** across all products. A cart that mixes currencies is rejected (`400`); the catalog feed, `llms.txt` and the discovery card all read from this file.

### 2. Add your payment keys

Copy `.env.example` to `.env` and set the Razorpay keys (see [Payments](#payments-razorpay)):

```bash
cp .env.example .env
# set RAZORPAY_KEY_ID, RAZORPAY_KEY_SECRET, RAZORPAY_MODE (sandbox|live),
# and RAZORPAY_WEBHOOK_SECRET (verifies payment.paid webhooks)
```

Register a webhook in the Razorpay dashboard pointing at your public `POST /webhooks/razorpay` (via your Caddy/Traefik proxy, see [Deploy with Docker](#deploy-with-docker)); the server verifies its HMAC and moves the referenced order to paid.

### 3. Build and run with your catalog

Because the catalog is compiled in, rebuild the image from your checkout:

```bash
docker build -t my-store .                     # compiles aiter-server with your fixture
docker run --rm -p 8080:8080 --env-file .env my-store
```

or use compose (it builds from `.` when the image is missing):

```bash
docker compose up -d
```

Run the signed demo buy against it (the demo agent is pre-registered in every build):

```bash
cargo run --bin aiter-cli -- --base http://localhost:8080
```

### 4. Register your own agents (not the demo one)

`agent-demo` is pre-registered with a fixed, public Ed25519 seed, so it is for demos and tests only. A live store should sign with its own agents. Clean agent registration (an admin surface that takes an Ed25519 public key plus a spend cap) is not wired yet; agents are currently registered in code on `AppState` (`crates/aiter-server/src/catalog.rs`). Until that lands, add your agents in code and set spend caps there.

### 5. What is still demo-grade

- **Catalog** is embedded at build time: change products by editing the fixture and rebuilding, not through a runtime API.
- **State is in-process** (see [Deploy with Docker](#deploy-with-docker)): carts, sessions, orders and consents live in memory, so a restart loses them. When the `sled`-backed store lands, mount a volume for the store directory.
- **Healthcheck is external**: the distroless image has no shell.

## License

MIT — see [LICENSE](LICENSE).