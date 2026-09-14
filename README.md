# mastoot

An aesthetically-driven Mastodon TUI client. Rust + ratatui. macOS first, Linux second.

> 在终端里刷 Mastodon 能不能接近 SwiftUI 原生客户端的视觉舒适度？
>
> Inspired by [Phanpy](https://github.com/cheeaun/phanpy) (UX) and
> [Ice Cubes](https://github.com/Dimillian/IceCubesApp) (visual density).

See [CLAUDE.md](./CLAUDE.md) for design philosophy, architecture, and the
implementation log.

## Status

Daily-usable. Home / Local / Federated / Notifications / Profile, threads,
compose / reply / quote / delete, favourite / boost / bookmark, follow lists,
multi-account switching, inline images (kitty / iTerm2 / Sixel / halfblocks),
SSE live updates with polling fallback, Apple Music link cards, polls,
link previews, search (accounts · hashtags · posts), Markdown-style
formatting from forks that send it.

Not done: custom-emoji images (shortcodes show dimmed), media upload,
lists, voting.

## Quick start

```bash
# Build (Rust 1.88+)
cargo build --release

# First run walks you through OAuth in the browser, then opens the TUI.
cargo run --release -- --instance mastodon.social
```

Other subcommands: `login`, `logout`, `whoami`, `accounts`, `switch <handle>`.

## Keys

| Key | Action |
| --- | --- |
| `1` `2` `3` `4` `5` | Home / Local / Federated / Notifications / your profile |
| `j` `k` `gg` `G` | move · top · bottom |
| `l` `Enter` | open thread |
| `h` `Esc` | back (at the top level, `Esc` asks before quitting; `Ctrl+C` quits at once) |
| `f` `b` `B` | favourite · boost · force un-boost |
| `c` `r` `q` | new post · reply · quote |
| `d` | delete your own post (confirms) |
| `u` | author's profile |
| `o` `y` | open in browser · copy link |
| `Q` | open the quoted post |
| `/` | search accounts, hashtags and posts (`Enter` on a hashtag opens its timeline) |
| `s` | reveal / hide a content warning |
| `R` | refresh |
| `S` `D` `A` | live-update mode · density · switch account |
| `Tab` | cycle notification filter |
| `F` `o` `O` | in a profile: follow · followers · following |
| `?` | help |

Compose: `Ctrl+Enter` send (also `Alt+Enter` / `Ctrl+D`), `Ctrl+W` visibility,
`Ctrl+S` content warning, `Tab` switch field, `Esc` cancel.

`Ctrl+Enter` needs the kitty keyboard protocol — WezTerm, kitty, Ghostty,
foot, Konsole and iTerm2 (with CSI-u enabled) all support it. On other
terminals use `Alt+Enter` or `Ctrl+D`.

## Config

`~/.config/mastoot/config.toml` (created on first login):

```toml
default_instance = "mastodon.social"
default_account = "you@mastodon.social"

[theme]
name = "frost"            # or "ember"

[ui]
nerd_font = true          # false → ASCII fallbacks for icons
show_relative_time = true # false → "Jan 15 14:32"
media_render = "auto"     # auto | images | text_only
stream_mode = "streaming" # streaming | polling | off
# image_protocol = "kitty"  # force kitty | iterm2 | sixel | halfblocks
```

Tokens live in the OS keyring, not in the file. Logs go to
`~/.cache/mastoot/log.txt` (`-v` / `-vv` for more).

## Terminal notes

- Images: kitty, WezTerm, Ghostty, iTerm2 render real pictures; everything
  else gets half-block art. `media_render = "text_only"` shows alt text only.
- Copy link (`y`) uses OSC 52, so it works over SSH in the terminals above.
- A Nerd Font is assumed for icons; set `nerd_font = false` otherwise.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
