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
link previews, search (accounts · hashtags · posts), threaded replies with
reply previews, favourites / bookmarks, post editing, Markdown-style
formatting from forks that send it.

Not done: custom-emoji images (shortcodes show dimmed), media upload,
lists, voting.

## Quick start

```bash
git clone git@github.com:ReflectionL/mastoot.git && cd mastoot
cargo build --release          # rustup picks up rust-toolchain.toml; needs Rust 1.88+
./target/release/mastoot --instance mastodon.social
```

The first run walks you through OAuth in your browser, stores the token in
the OS keyring, and opens the TUI. After that a bare `mastoot` is enough.
The config file lives at `~/.config/mastoot/config.toml` on Linux and
`~/Library/Application Support/io.github.reflectionl.mastoot/config.toml`
on macOS; tokens are never written to it.

macOS asks once whether mastoot may read the keychain — choose **Always
Allow**, or it asks again on every launch.

Other subcommands: `login`, `logout`, `whoami`, `accounts`, `switch <handle>`.

### Headless / over SSH

Two things differ on a server without a desktop:

- **Keyring.** On Linux the token is stored through Secret Service
  (gnome-keyring, KDE Wallet). A bare server usually has none, and login
  will fail when it tries to save the token. Start one first, e.g.
  `dbus-run-session -- sh -c 'gnome-keyring-daemon --components=secrets --unlock; mastoot'`.
  There is no file-based fallback yet.
- **OAuth callback.** `mastoot login --no-browser` prints the authorization
  URL for you to open locally, but the browser then redirects to
  `127.0.0.1:<port>/callback` — a port on the *server*. Forward it before
  logging in, using the port shown in the `login` output:
  `ssh -L <port>:127.0.0.1:<port> your-server`.

## Keys

Press `?` in the app for the same list.

<!-- keys:start (generated from src/ui/keymap.rs) -->

**Anywhere**

| Key | Action |
| --- | --- |
| `?` | toggle this help |
| `Esc` | back; at the top level asks before quitting |
| `Ctrl+C` | quit at once |
| `1` `2` `3` `4` | Home / Local / Federated / Notifications |
| `5` | your profile |
| `6` `7` | your favourites / bookmarks |
| `/` | search accounts · hashtags · posts |
| `A` | switch account |
| `S` | cycle live updates: streaming · polling · off |
| `D` | toggle inter-post density (1 ↔ 2 blank lines) |

**Moving around**

| Key | Action |
| --- | --- |
| `j` `↓` | next post |
| `k` `↑` | previous post |
| `gg` `G` | top / bottom |
| `R` | refresh |
| `l` `Enter` | open thread |
| `h` `Esc` | back |
| `Q` | open the quoted post |
| `s` | reveal / hide a content warning |
| `Tab` `Shift+Tab` | cycle the notification filter |

**On a post**

| Key | Action |
| --- | --- |
| `f` | favourite / unfavourite |
| `b` | boost / unboost |
| `B` | force un-boost |
| `c` | new post |
| `r` | reply |
| `q` | quote (Mastodon 4.5+ native quote) |
| `e` | edit your own post |
| `d` | delete your own post (Enter confirms) |
| `u` | author's profile |
| `o` | open in browser |
| `y` | copy link |

**In a profile**

| Key | Action |
| --- | --- |
| `F` | follow / unfollow |
| `o` `O` | followers / following |

**Composing**

| Key | Action |
| --- | --- |
| `Ctrl+Enter` | send (Alt+Enter and Ctrl+D also work) |
| `Esc` | cancel (asks first if there is a draft) |
| `Ctrl+W` | cycle visibility |
| `Ctrl+S` | toggle content warning |
| `Tab` | switch between body and content warning |
<!-- keys:end -->

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

## Development

```bash
cargo test                      # unit tests + golden-screen tests (no terminal needed)
cargo clippy --all-targets -- -D warnings
```

`scripts/tui_snapshot.py` drives the real binary in a pseudo-terminal and
dumps the screen as text (`pip install pyte`); handy for eyeballing a change
against a live account. Each rebuilt binary re-triggers the macOS Keychain
prompt; `scripts/codesign-dev.sh` signs it with a self-signed identity so the
approval sticks (setup steps in the script).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
