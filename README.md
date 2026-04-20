# mastoot

An aesthetically-driven Mastodon TUI client. Rust + ratatui. macOS first.

> 在终端里刷 Mastodon 能不能接近 SwiftUI 原生客户端的视觉舒适度？
>
> Inspired by [Phanpy](https://github.com/cheeaun/phanpy) (UX) and
> [Ice Cubes](https://github.com/Dimillian/IceCubesApp) (visual density).

See [CLAUDE.md](./CLAUDE.md) for design philosophy, architecture, and
implementation roadmap.

## Status

🚧 **Phase 1 in progress** — API SDK + OAuth. Not usable as a TUI yet.

## Quick start

```bash
# Install Rust if needed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build

# Try the Phase 1 verification example (interactive OAuth in your browser):
cargo run --example fetch_home
```

## Default instance

`mastodon.social` — override with `--instance your.server`.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
