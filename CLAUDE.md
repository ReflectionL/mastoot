# CLAUDE.md — mastoot

A Mastodon TUI client. Rust + ratatui. macOS first.

## 1. 项目定位

一个审美驱动的 Mastodon 终端客户端。核心判据不是功能完备度，而是
**"在终端里刷 Mastodon 能不能接近 SwiftUI 原生客户端的视觉舒适度"**。

参考坐标：
- **Phanpy** (UX 哲学)：隐藏 action buttons、alt text inline、reply preview、catch-up 模式
- **Ice Cubes** (信息密度与配色)：timeline 上 avatar + 用户名 + 正文的层级关系
- **不参考**：toot (urwid 老派)、Mastui (多列拥挤)

设计原则：**减法优先**。能隐藏的 UI 元素都隐藏，进入详情页再展开。
审美参照：古典乐录音里的 Michelangeli / Karajan Mozart Requiem 的那种克制、
结构密度、内省感。配色与字体层次要服务于这种"留白比装饰更重要"的取向。

运行平台：macOS (主要)，Linux (次要，用于 SSH 到远程服务器使用)。
假设用户终端为 WezTerm / iTerm2 / kitty / Ghostty 其中之一，装有 Nerd Font
（默认假设 JetBrains Nerd Font，但不依赖特定字体文件）。

## 2. Tech Stack

- **Language**: Rust (edition 2024, MSRV = 1.82)
- **TUI**: ratatui ≥ 0.30 + crossterm
- **Async runtime**: tokio (full features)
- **HTTP**: reqwest (rustls-tls only, 禁用 openssl 避免 macOS 链接问题)
- **SSE streaming**: reqwest-eventsource
- **JSON**: serde + serde_json
- **Data models**: 纯 serde struct
- **HTML parsing**: scraper crate (解析 Mastodon HTML 子集)
- **Image rendering**: ratatui-image (自动协议检测：kitty > iTerm2 > Sixel > halfblocks)
- **Auth storage**: keyring crate（macOS Keychain）
- **Config**: toml + directories crate（~/.config/mastoot/config.toml）
- **CLI parsing**: clap (derive macros)
- **Logging**: tracing + tracing-subscriber（写文件，不污染 TUI）
- **Error**: anyhow (application) + thiserror (library boundaries)
- **Secret handling**: secrecy crate（token 在内存中标记为 zeroize-on-drop）

**明确不用**：
- SQLite / 任何持久化数据库
- tui-realm / rat-salsa 等 ratatui 之上的封装
- openssl（用 rustls）

## 3. 架构

三层，严格单向依赖：

```
┌─────────────────────────────────────┐
│  ui/          (ratatui widgets)     │
├─────────────────────────────────────┤
│  state/       (AppState + events)   │
├─────────────────────────────────────┤
│  api/         (reqwest + SSE)       │
└─────────────────────────────────────┘
```

**关键原则**：
- `api` 不知道 TUI 存在，可独立当 SDK 使用
- `state` 和 `ui` 之间通过 `tokio::sync::mpsc` 通信：UI → Action，State → Event
- `ui` 只做渲染和输入分发，状态变更一律发事件给 state 层

### 3.1 目录结构

单 crate，不搞 workspace。

```
mastoot/
├── Cargo.toml
├── rust-toolchain.toml
├── CLAUDE.md
├── README.md
├── src/
│   ├── main.rs                 # 入口
│   ├── cli.rs                  # clap derive
│   ├── config.rs               # TOML 读写、账号管理
│   ├── icons.rs                # Nerd Font glyph constants
│   ├── api/
│   │   ├── mod.rs
│   │   ├── auth.rs             # OAuth2 授权码流程
│   │   ├── client.rs           # reqwest Client 封装
│   │   ├── models.rs           # Status, Account, Notification...
│   │   ├── endpoints.rs        # 所有 REST 方法
│   │   ├── streaming.rs        # SSE user stream
│   │   └── html.rs             # Mastodon HTML → ratatui Text
│   ├── state/
│   │   ├── mod.rs
│   │   ├── app.rs              # AppState
│   │   ├── timeline.rs         # TimelineStore（Vec + dedup by id）
│   │   └── event.rs            # Action / Event enum
│   └── ui/
│       ├── mod.rs
│       ├── app.rs              # 主循环 + screen router
│       ├── theme.rs            # 配色和字体层次
│       ├── screens/
│       │   ├── timeline.rs
│       │   ├── status_detail.rs
│       │   ├── compose.rs
│       │   ├── notifications.rs
│       │   └── profile.rs
│       └── widgets/
│           ├── status_card.rs  # 核心组件
│           ├── reply_preview.rs
│           ├── alt_text.rs
│           ├── media.rs        # 图片/视频占位与实际渲染
│           └── help.rs
├── examples/
│   └── fetch_home.rs           # 阶段 1 验收用
└── tests/
    └── api_integration.rs      # 需要真实账号，#[ignore] 默认跳过
```

## 4. MVP 范围

### 4.1 Must have

- OAuth 登录单账号（默认 instance `mastodon.social`，可 `--instance` 覆盖）
- Home / Local / Federated / 单用户 profile timeline
- Status detail 页（含完整回复链）
- 发帖 / 回帖 / 点赞 / 转发 / 收藏
- 通知 timeline（mention / favorite / boost / follow 分类）
- Alt text inline 显示
- Reply preview（timeline 里看到回复时显示被回复的那条）
- **媒体图片渲染**：kitty / iTerm2 协议下显示真实图片；fallback 到半块字符或 alt text
- Vim-like 键位
- 默认主题（冷色系，克制型）+ 一个备选主题
- Streaming user stream：home timeline 实时追加新帖
- Nerd Font 图标用于 action markers（boost / favorite / reply）

### 4.2 明确不做（Non-goals）

- 推送通知
- 任何本地数据库 / 离线缓存
- 多账号切换（v2）
- 管理员工具
- 跨设备 marker 同步
- 列表 / followed tags / tag groups
- 投票创建（查看可以）
- Quote post
- 自定义表情上传
- 视频/音频播放（显示占位符 + 链接即可）

### 4.3 可能做（Stretch）

- Catch-up 模式（类 Phanpy）
- 配置文件热重载
- 导出单条 status 为 markdown

## 5. 默认账号与 Instance

```toml
# ~/.config/mastoot/config.toml 示例
default_instance = "mastodon.social"

[theme]
name = "frost"       # 或 "ember"

[ui]
show_relative_time = true
media_render = "auto"  # auto | images | text_only
nerd_font = true
```

OAuth scope: `read write follow`（不申请 `push`，我们不做推送）。
Token 存 keyring，service name `mastoot`，account 为 `{username}@{instance}`。

## 6. API 层设计

### 6.1 Client

```rust
pub struct MastodonClient {
    base_url: Url,
    token: SecretString,
    http: reqwest::Client,
}

impl MastodonClient {
    pub fn new(instance: &str, token: SecretString) -> Result<Self>;
    pub async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T>;
    pub async fn post<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T>;
    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T>;
    // 429 rate limit 自动指数退避（遵循 X-RateLimit-Reset header）
    // 默认 10s timeout，streaming 除外
    // User-Agent: mastoot/{version} (+https://github.com/<user>/mastoot)
}
```

### 6.2 Models

所有模型都是纯 serde struct，`#[serde(rename_all = "snake_case")]`。

关键类型：
- `Status` — 帖子（含可能的 `reblog: Box<Status>`）
- `Account` — 用户
- `Notification` — 通知
- `MediaAttachment` — 附件（type: image / video / gifv / audio）
- `Context` — 回复链（ancestors + descendants）
- `Card` — 链接预览
- `Application` — 发帖来源（显示为 "via X"）

### 6.3 HTML 解析策略

Mastodon 返回的 `content` 是受限 HTML 子集：
`<p>`, `<br>`, `<a>`, `<span class="mention">`, `<span class="hashtag">`,
`<span class="invisible">`, `<span class="ellipsis">`.

`api/html.rs` 用 scraper 解析 DOM → 遍历生成 `Vec<Line<'static>>`。
Mention 和 hashtag 用主题色高亮，URL 的 `invisible` 前缀（比如 `https://`）
渲染为 dim 色。

### 6.4 Streaming

基于 `reqwest-eventsource` 的 SSE stream，封装成 `Stream<Item = StreamEvent>`。
断线重连由 state 层监听 `Error` 后重启 task，指数退避从 1s 到 30s。

事件类型：`Update(Status)`, `Delete(StatusId)`, `Notification(Notification)`,
`StatusUpdate(Status)`, `Disconnect`.

## 7. UI 层设计要点

### 7.1 Status Card

Timeline 上的单条 post 渲染。**这是整个应用的视觉核心，要反复打磨**。

布局草图（Nerd Font 图标用占位符表示）：
```
  Thomas Ricouard  @dimillian@mastodon.social · 2h

  Working on some new features for Ice Cubes this
  weekend. The @textual framework is really nice.

  [🖼  image rendered here if supported, or]
  󰋩  Screenshot of new settings panel

  ↪  replying to @someone: "What about..."

```

规则：
- 默认不显示 action buttons（reply/boost/favorite 数字）
- 进入 detail 时才展开 action bar
- Boost 用 Nerd Font 箭头图标（`nf-md-repeat` = `󰑖`）前缀一行："󰑖 @user boosted"
- Reply preview 仅在该帖是 timeline 第一条出现的回复时显示
- 时间戳用相对时间（2h / 3d），超过 7 天显示日期（Jan 15）
- 帖子之间用留白分隔（1 空行），不画边框
- 选中态用左边一条竖线 `▏` 作为 cursor，不整行反白

### 7.2 Nerd Font 图标约定

`src/icons.rs`:
```rust
// 使用 Material Design Icons (nf-md-*) 范围，JetBrains Nerd Font 支持
pub const BOOST: &str        = "\u{f01e6}";  // 󰇦 nf-md-repeat_variant
pub const FAVORITE: &str     = "\u{f04ce}";  // 󰓎 nf-md-star
pub const REPLY: &str        = "\u{f0167}";  // 󰅧 nf-md-reply
pub const BOOKMARK: &str     = "\u{f00c0}";  // 󰃀 nf-md-bookmark
pub const IMAGE: &str        = "\u{f0976}";  // 󰥶 nf-md-image
pub const VIDEO: &str        = "\u{f05a0}";  // 󰖠 nf-md-video
pub const GIF: &str          = "\u{f0a0f}";  // 󰨏 nf-md-file_gif_box
pub const LOCK: &str         = "\u{f033e}";  // 󰌾 nf-md-lock
pub const VERIFIED: &str     = "\u{f05e1}";  // 󰗡 nf-md-check_decagram
pub const LINK: &str         = "\u{f0337}";  // 󰌷 nf-md-link_variant
pub const WARNING: &str      = "\u{f0026}";  // 󰀦 nf-md-alert
pub const NOTIFICATION: &str = "\u{f009a}";  // 󰂚 nf-md-bell
```

若用户在 config 里 `nerd_font = false`，所有图标降级为 ASCII：`[boost]` / `*` / `↪` 等。

### 7.3 主题

ratatui `Style` 封装到 `Theme` struct 里。所有颜色和强调层次都必须走这个 struct。

**Frost 主题**（默认，冷色克制型）：
```rust
Theme {
    // 三级前景色层次
    fg_primary:   Color::Rgb(225, 227, 235),
    fg_secondary: Color::Rgb(148, 154, 172),
    fg_tertiary:  Color::Rgb( 95, 100, 120),

    // 功能色
    accent:   Color::Rgb(124, 158, 255),   // 链接 / cursor
    mention:  Color::Rgb(184, 166, 255),   // @user
    hashtag:  Color::Rgb(143, 207, 191),   // #tag
    boost:    Color::Rgb(143, 207, 143),
    favorite: Color::Rgb(255, 196, 120),
    error:    Color::Rgb(255, 120, 120),

    // 背景（默认透明，让终端背景透过来）
    bg: Color::Reset,
}
```

**Ember 主题**（备选，暖色内敛型）：
类似层次结构，以暖灰 + 铜色 accent 为主。

主题切换通过 config。不做运行时热切换（v2 再加）。

### 7.4 键位

**全局**：
| Key | Action |
|-----|--------|
| `?` | 帮助 |
| `q` | 退出 |
| `1`/`2`/`3`/`4` | Home / Local / Federated / Notifications |

**Timeline 内**：
| Key | Action |
|-----|--------|
| `j`/`k` | 下/上移动 |
| `gg`/`G` | 首/尾 |
| `l` / `Enter` | 打开详情 |
| `h` / `Esc` | 返回 |
| `r` | 回复当前帖 |
| `f` | 收藏 |
| `b` | 转发 |
| `B` | 取消转发 |
| `c` | 新帖 |
| `/` | 搜索 |
| `R` | 强制刷新 |
| `o` | 在浏览器打开当前帖 |
| `y` | 复制链接 |

**Compose 内**：
| Key | Action |
|-----|--------|
| `Ctrl+Enter` | 发送 |
| `Esc` | 取消（有确认弹窗） |
| `Ctrl+W` | 设置可见性 |
| `Ctrl+S` | 切换敏感内容标记 |

## 8. 代码规范

- Rust edition 2024
- CI 开 `-D warnings`
- clippy level: `clippy::pedantic` + 选择性 `allow`（模块顶部注释说明原因）
- 所有公开 API 有 doc comment
- `api/` 层禁止 `println!` / `eprintln!`（会污染 TUI）
- 所有日志走 `tracing`，写 `~/.cache/mastoot/log.txt`
- 单元测试：`api/` 和 `state/` 必须有，`ui/` 可以放后
- 集成测试需要真实账号，用 `#[ignore]` 默认跳过
- 不用 `unsafe`。如果确实需要，需在 PR 里写清楚 SAFETY 理由

## 9. 实现顺序

**阶段 1：项目骨架与 API SDK**
1. `cargo new mastoot` + Cargo.toml 依赖 + rust-toolchain.toml
2. CI（github actions：fmt + clippy + test）
3. `api/models.rs` 全部 Mastodon 实体
4. `api/client.rs` + `api/endpoints.rs`（至少 timeline / status / account 相关）
5. `api/auth.rs` OAuth 授权码流程（本地 127.0.0.1 redirect 回调）
6. `examples/fetch_home.rs`：登录 → 拉 10 条 home → 纯文本打印
7. **Checkpoint**：能跑通再进阶段 2

**阶段 2：最小 TUI**
8. `ui/app.rs` 骨架 + 主循环 + screen router
9. `theme.rs` 和 `icons.rs`
10. `widgets/status_card.rs` 单条渲染（纯文本，无图片）
11. `screens/timeline.rs` home timeline 能滚动
12. HTML → ratatui Text 转换
13. **Checkpoint**：视觉基本成型再进阶段 3

**阶段 3：交互**
14. Compose screen + 发帖 API
15. 点赞 / 转发 / 回帖
16. Status detail + 回复链渲染
17. 通知页
18. **Checkpoint**：日常可用

**阶段 4：打磨**
19. ratatui-image 集成，媒体 widget
20. Streaming 实时更新
21. 主题系统完善 + ember 主题
22. 帮助页 / 键位提示
23. 错误处理完善（网络断开 toast 等）

## 10. 开发环境 Bootstrap（macOS）

```bash
# 1. 装 Rust（如果还没装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# 2. 装常用 cargo 扩展
cargo install cargo-watch cargo-nextest cargo-edit

# 3. Clone 后
cd mastoot
rustup show   # 自动装 toolchain
cargo build

# 4. 开发流
cargo run -- --help
cargo watch -x 'clippy --all-targets'
cargo nextest run
cargo run --example fetch_home
```

## 11. 非规范性参考

- Mastodon API: https://docs.joinmastodon.org/
- Ratatui docs: https://ratatui.rs/
- Ratatui awesome list: https://github.com/ratatui/awesome-ratatui
- ratatui-image: https://github.com/benjajaja/ratatui-image
- Phanpy source: https://github.com/cheeaun/phanpy
- Ice Cubes source: https://github.com/Dimillian/IceCubesApp
- Nerd Font cheatsheet: https://www.nerdfonts.com/cheat-sheet

---

## 12. 剩余 Open Questions

1. **GitHub repo 名和 URL**：`https://github.com/ReflectionL/mastoot`（User-Agent 和 OAuth `website` 字段已用此值）
2. **LICENSE**：MIT / Apache-2.0 / dual？
   - 暂定：dual MIT / Apache-2.0。`LICENSE-MIT` 已就位；后续需要补 `LICENSE-APACHE`
3. **一个 fallback 终端场景**：如果用户终端既不支持 kitty 也不支持 iTerm2 图片协议，且关闭了半块渲染，媒体怎么显示？我建议显示 `[󰥶 image: {alt}] (press v to view in browser)` 这种纯文本占位
4. **初次启动流程**：没有 config 和 token 时，直接进 OAuth 向导？还是要求用户先跑 `mastoot login`？我倾向前者，零配置体验更好
   - 暂定：零配置流程。`mastoot run` 无 token 时提示并退出，用户按提示跑 `mastoot login`；Phase 2 会改成自动进 OAuth 向导

## 13. 进度跟踪

> **规则**：每个有用户可见变更的改动（step / polish / bugfix / 审美调整）
> 完工时必须往 [`UPDATE_LOG.md`](./UPDATE_LOG.md) 顶部 prepend 一条详细条目
> （起因 / 改动 / 决策坑），reverse-chronological。本节（§13）只维护
> **phase 级里程碑 + 每个子步骤一行 bullet**；别把详情塞进来。
>
> 触发点：一次 iteration 结束 → build/clippy/test 跑过 → 同时更新这两个
> 文档再收工。漏记会让下一次 compact 丢上下文（已经发生过一次）。

### Phase 5  多账号 + quote 完整链路  ·  2026-04-18

- Splash · 2026-04-20 — 冷启动 `App.splash=true` 时只画居中一行 `mastoot.`（secondary 色）；`handle_key` 在 Ctrl+C 之后直接 `return true` 吞所有键；`handle_event` 命中 `TimelineUpdated{Home}` 或 `ApiHealthChanged(非 Healthy)` 时 `splash=false`——成功 path 无缝切到填好的 Home，失败 path 也不卡死

用户侧三个请求：(1) 多账号切换，(2) Fedibird-style `RE: <url>` 从 quote 帖正文剥掉
+ `Q` 跳转，(3) 发原生 quote post（`q` 键）。

- CLI · `mastoot accounts` 列出所有登录账号（`*` 标当前），`mastoot switch <handle>` 设 default_account；两者与现有 `login` / `logout` 无缝配合（`login` 已往 `accounts` 列表追加）
- TUI · 全局 `A` 键开 account switcher modal；`Enter` 确认 → UI 读 keyring token → `Action::SwitchAccount` → state task 重建 client + 重置 AppState + 重启 live slot + load Home；配置文件里 `default_account` 同步落盘。挑当前账号时 no-op 关闭 modal
- 并发原语：零共享锁。state task 本来就是 `MastodonClient` 唯一 owner，`streaming_loop` 拿的是 `.clone()`；swap 时 `live.shutdown()` 把旧 clone 回收，`live.set(mode)` 用新 `&client` 重新 clone。`Arc<RwLock>` / `ArcSwap` 两个方案都是伪需求
- Quote `RE:` 剥离 · `strip_re_reference` in `status_card::build_lines`：用已有的 `render_with_links` 拿到 link 位置，match `href == quoted.url/uri` 或 `/{quoted_id}` 结尾，line 的前后文 trim 后是纯 `re:` / `qt:` / 全角变体才整行删；误伤测试 `quote_keeps_re_inside_prose` 保证 prose "context: RE: the prior" 不动
- Quote 跳转 · 全局 `Q` 键（compose 外）从 Timeline / Detail / Profile 任何模式下 cursor 所在 status 的 `quote.quoted_status` 克隆一份、push `Mode::StatusDetail` + `OpenStatus`；`quote_block` header 末尾加 `· Q: open` 暗字提示
- 发 Quote · 小 `q` 键 = quote selected（Timeline / Detail / Profile 三个模式都接）。API 走 Mastodon 4.5 原生 `POST /api/v1/statuses` 带 `quoted_status_id`（Rust 内部字段名 `quote_id`，serde `rename = "quoted_status_id"`——原按 `quote_id` 发被 server 静默忽略，对着官方 docs 改）；`ComposeState::quote(ctx, max_chars)` body 起始空、visibility 默认 Public（不继承父帖——quote 是独立评论）；compose 顶部 preview 区复用 reply 布局，header 换 `Quoting @...` + `❝` glyph。退出改 `Esc`/`Ctrl+C`（让出 `q` + 防误按：`q`/`Q` 都是"打开/写"而不是"退出"）
- 删帖 · 小 `d` 键 = delete selected（Timeline / Detail / Profile 三处都接，只对 `me.id == target.account.id` 生效，其它情况静默）。`Mode::DeleteConfirm(StatusId)` modal 居中红框 `Delete this post? Enter: delete · Esc: cancel`；成功 `Event::StatusDeleted(id)` 裁掉所有 timeline 里的该条 + 若用户正盯着该条 focal 的 detail 也自动 pop_mode 返上级。复用 `client.delete_status` endpoint（早就写好了，只是没被 UI 接过）

### Phase 1 ✅  项目骨架 + API SDK  ·  2026-04-17

完整 Mastodon REST SDK、OAuth（PKCE S256）、HTML 解析、SSE streaming、keyring 存 token、
CLI `run/login/logout/whoami`；`examples/fetch_home` 对真实实例实测跑通。

### Phase 2 ✅  最小 TUI  ·  2026-04-17

ratatui 主循环（crossterm raw mode + panic hook + 3 路 select）、state task（tokio + mpsc）、
status card widget、timeline j/k/gg/G/R 滚动与触底自动分页、零配置 OAuth 入口。

- Polish #1 · 2026-04-17 — 修复帖内/帖间间距层级颠倒；加 CJK-aware 自动换行
- Polish #2 · 2026-04-17 — 去掉 polish #1 加的 `─` 分隔线，帖间改用 2 空行纯留白（原则：留白 > 装饰）

### Phase 3  交互  ·  in progress

Compose + `post_status`；`f` / `b` / `r` 键位 → favourite / reblog / reply；
status detail 页（含回复链）；notifications 分类页。

- Step 1 · 2026-04-17 — `f` / `b` / `B` 键位 + 乐观本地更新；viewer-state 标记（头行末尾加 `󰓎` / `󰇦` / `󰃀`，无计数）
- Step 2 · 2026-04-17 — Compose + `c` / `r` 键位；自写 textarea widget；`Ctrl+Enter` 发送；过字数/空草稿阻止提交；成功后自动刷 home
- Polish #1 · 2026-04-17 — CW 单行容器里吞 Enter；`Ctrl+S` 不再抢焦点（保留正文焦点）；CW label 加 `●` / `(Tab to focus)` 指示；状态栏右对齐 hint
- Step 3 · 2026-04-17 — Status detail 页（`l` / `Enter` 进，`h` / `Esc` 出）；`/context` 拉 ancestors + descendants 拍平展示；`f` / `b` / `r` 在 detail 内对游标指向的 status 生效（boost 走内层）
- Polish #2 · 2026-04-17 — 详情页 focal 加计数行；`s` 键 CW 折叠/展开；启动拉 `/api/v2/instance` 拿真实 max_chars；detail→reply→回 detail（不丢回 timeline）；action 失败自动回滚 optimistic 状态
- Step 4 · 2026-04-17 — Notifications 页（`4` tab）；`Tab` / `Shift+Tab` 切 filter（All / Mentions / Boosts / Favourites / Follows）；客户端过滤 + max_id 分页；`l` / `Enter` 进关联 status detail
- Step 5 · 2026-04-17 — 帖间距 2→1（撤销层级，换信息密度）；Quote 显示（typed `QuoteData`，嵌入卡片，dim+indent）；`5 Profile` tab 自己 + `u` 键打开他人 profile（modal，`h`/`Esc` 返回）
- Polish #3 · 2026-04-17 — 通用 `back_stack: Vec<Mode>` 替换 `pending_return`；任何 sub-page 都能正确 pop 回上级（修 profile→l→h 跳错）；`D` 键运行时切 inter-post 间距 1↔2

### Phase 4 ✅  打磨  ·  2026-04-18

全部 7 个子步骤完工：ratatui-image 实图（timeline / profile / detail 全屏铺开）、
SSE user stream + 三档 live update（streaming / polling / off, `S` 键 + config）、
Follow 与 Followers / Following 列表、Apple Music 富显示（iTunes API + 封面图）、
网络错误分类 + 状态栏 connection 指示、ember 主题校色。测试 94 通过，clippy 干净。

按 `E → F → G → A3 → C → B → D` 顺序：
- **A** ratatui-image 实图（A1/A2/A3 done）
- **B** UserStream SSE 接入（指数退避重连，状态栏指示真实状态）
- **C** 网络错误分类 toast + 状态栏 connection 健康度
- **D** ember 主题校色（暖灰 + 铜色）
- **E** Follow / Unfollow + relationship chip in profile（`F` 键）
- **F** Followers / Following 列表（`o` / `O` 在 profile 内打开 modal sub-page，`l/Enter` 钻进对方 profile）
- **G** Apple Music 链接富显示 — **进阶版**：iTunes Search API 拿 metadata + 封面图通过 ImageCache 真渲染。要做得好看，是项目美学的延伸

- A1 · 2026-04-17 — `ImageCache`（picker autodetect + tokio 异步下载 + decode）；iTerm2/kitty/Sixel/halfblocks 协议；`TERM_PROGRAM` 强制 iTerm2 修 picker 误判
- A2 · 2026-04-17 — Detail focal status 渲染图片（status_card 改 `render_blocks` 返回 lines + image_overlays；screen 在 Paragraph 渲染后叠 `StatefulImage`）
- E · 2026-04-17 — `LoadRelationship` 进 profile 自动拉；`F` 键 toggle follow（乐观 + 失败回滚）；header chip 显示 mutual / following / requested / follows you
- F · 2026-04-17 — `account_followers` / `account_following` endpoints + `Action::LoadAccountList` / `Event::AccountListLoaded`；新 `AccountListScreen` + `Mode::AccountList`（紧凑 2 行卡片）；profile 内 `o` / `O` 打开 followers / following，`l`/`Enter` 钻进对方 profile（push 进 back_stack）
- G · 2026-04-17 — 进阶版 Apple Music：`api/music.rs`（URL 解析 song/album/playlist/artist + `?i=` 深链 → iTunes Lookup API → `AppleMusicMeta` 缓存）；cover art 走 `ImageCache`（`MediaId::new("music:{url}")` 复用缓存）；`status_card` 两档显示（compact `󰝚 · Title · Artist` / spacious 卡片带封面 + 变高多行文字），`D` 键切换；timeline / profile / detail 都显示封面（A3 的一部分，music overlay 复用 `images::draw_overlay`）
  - G · 点击支持尝试（OSC 8 via `frame.buffer_mut` 覆盖 + UNDERLINED marker 扫描）失败——ratatui cell flush 会在 cells 间插 cursor move 切断转义序列，Nerd Font PUA 字符 `unicode-width` 返回 None 让 span 宽度计算失准。撤销，文字正常渲染；click 留到未来（task #74）
- C · 2026-04-18 — `ApiError::category()` + `terse()` 分类（offline/timeout/rate/auth/404/5xx/client）；`ApiHealth` + `Event::ApiHealthChanged`；状态栏左点跟随 health 变色，非 healthy 时带后缀 label（`offline` / `degraded` / `login?`）；toast 去掉 reqwest 内部术语改成人话
- B · 2026-04-18 — state task spawn `streaming_loop`（指数退避 1s→30s cap，成功连接重置）；`StreamEvent::Update` → `Event::TimelineStatusAdded` prepend 到 Home；`Notification` → 现有 `NotificationReceived` 走 prepend；`Delete` / `StatusUpdate` 路由回已有事件；状态栏在 api healthy 时才显示 stream 次级 label（`live off` / `reconnecting`）；TimelineScreen / NotificationsScreen 加 `on_prepended` 保持游标跟随用户正看的那条
- B toggle · 2026-04-18 — 三档 `StreamMode { Streaming, Polling, Off }`（config `[ui] stream_mode` + 运行时 `S` 键循环）；`LiveUpdateSlot` 管理单活 handle，切模式时 abort 旧 spawn 新；Polling = 每 30s 发 `Action::FetchNewer(Home)` 走 `since_id` prepend 新帖；状态栏左点 glyph 编码模式（`●` streaming / `…` polling / `·` off），颜色继续编码 api_health
- A3 · 2026-04-18 — `show_images` 在 timeline / profile / detail-非 focal 全开（gated on `images.enabled()`）；三个 screen 的 image overlay 遍历逻辑已经存在（之前给 music 卡片用的），只要 status_card 生成 overlay 就自动渲染
- D · 2026-04-18 — ember 主题校色：`mention` 从 copper 改成 dusty rose（和 accent 拉开 hue 距离）；`boost` 推绿、`hashtag` 推金（两个 olive 不再撞色）；`fg_tertiary` 亮一档防暗色终端吞细节；palette 整体还在"暖灰 + 铜色"envelope 内
