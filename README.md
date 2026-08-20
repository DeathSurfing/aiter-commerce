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

Early scaffold. The library and server skeleton compile; protocol work is next (see `crates/aiter-core`).

## Run

```bash
cargo run -p aiter-server
```

Then:

- `GET /` — service info
- `GET /agentic/health` — liveness + version

## License

MIT — see [LICENSE](LICENSE).
