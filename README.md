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

## Status

Early scaffold. The library and server skeleton compile; work is tracked in [GitHub milestones](https://github.com/DeathSurfing/aiter-commerce/milestones).

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

## Agent catalog surface (`aiter-server`)

The server exposes an agent-readable catalog + discovery surface (Day 1):

| Endpoint | Purpose |
|---|---|
| `GET /catalog/products` | Paginated, filterable catalog feed (envelope with `items`/`total`/`has_more`) |
| `GET /catalog/products/{id}` | Single product; `404` when unknown |
| `GET /.well-known/agent-card.json` | A2A-style merchant discovery card (capabilities/endpoints, absolute URLs) |
| `GET /llms.txt` | Deterministic, LLM-readable catalog export in the `llms.txt` convention |

### `GET /catalog/products` schema

Returns a paginated **envelope** — `{ items, total, limit, offset, has_more }` —
so a client can walk every page and know when to stop. `items` is the array of
`aiter-core` `Product` objects (same shape as the core `Product`
serialization), ordered by `id` unless `?search=` re-ranks them:

```json
{
  "items": [
    {
      "id": "latte",
      "title": "Caffè Latte",
      "price": { "units": 480, "currency": "USD" },
      "description": "Espresso with steamed milk.",
      "tags": ["coffee", "drink", "hot"],
      "image_url": null,
      "available_qty": 50,
      "variant": null
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

## Run

```bash
cargo run -p aiter-server
```

Then:

- `GET /` — service info
- `GET /agentic/health` — liveness + version

### Demo agent (issue #29)

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

## License

MIT — see [LICENSE](LICENSE).
