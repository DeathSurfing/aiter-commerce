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

## Run

```bash
cargo run -p aiter-server
```

Then:

- `GET /` — service info
- `GET /agentic/health` — liveness + version

## License

MIT — see [LICENSE](LICENSE).
