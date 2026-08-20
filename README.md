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

Work is planned in themed, versioned milestones (order matters, each builds on the previous).

| Milestone | Focus | Target |
|---|---|---|
| [v0.1.0](https://github.com/DeathSurfing/aiter-commerce/milestone/1) | Core data model & foundations | Sep 2026 |
| [v0.2.0](https://github.com/DeathSurfing/aiter-commerce/milestone/2) | Agent-facing catalog & discovery | Sep 2026 |
| [v0.3.0](https://github.com/DeathSurfing/aiter-commerce/milestone/3) | Cart & checkout flow | Oct 2026 |
| [v0.4.0](https://github.com/DeathSurfing/aiter-commerce/milestone/4) | Payments rail: Razorpay | Oct 2026 |
| [v0.5.0](https://github.com/DeathSurfing/aiter-commerce/milestone/5) | Agent identity & trust | Nov 2026 |
| [v0.6.0](https://github.com/DeathSurfing/aiter-commerce/milestone/6) | MCP surface & end-to-end demo | Dec 2026 |
| [v1.0.0](https://github.com/DeathSurfing/aiter-commerce/milestone/7) | Hardening & production readiness | Feb 2027 |

New contributors: start with a `good first issue` in the next milestone.

## Run

```bash
cargo run -p aiter-server
```

Then:

- `GET /` — service info
- `GET /agentic/health` — liveness + version

## License

MIT — see [LICENSE](LICENSE).
