//! Compose screen — new post or reply.
//!
//! Modal: when active, the timeline view is replaced entirely (rather
//! than overlaid) — composing is a focused task and the split-screen
//! feel would be noisy.
//!
//! Layout:
//!
//! ```text
//! Compose                                              42 / 500
//!
//! Replying to @author
//! > …first lines of parent status…
//!
//! CW: (Ctrl-S to toggle)                           (hidden if off)
//! …spoiler text…
//!
//! …body…
//! █
//!
//! visibility: public          Ctrl+Enter send · Esc cancel
//!                             Ctrl+V visibility · Ctrl+S CW
//! ```
//!
//! Keys handled:
//! - `Ctrl+Enter`, `Alt+Enter`, `Ctrl+D` → submit
//! - `Esc` → cancel (the app layer decides whether to show a confirm)
//! - `Ctrl+V` → cycle visibility
//! - `Ctrl+S` → toggle CW field (and focus it on toggle-on)
//! - `Ctrl+O` → attach a file: a one-line path prompt, then an
//!   optional alt-text prompt (`Enter` skips); `Ctrl+X` drops the last
//!   attachment
//! - `Tab` / `Shift+Tab` → switch focus body ↔ spoiler
//! - everything else routed to the focused text area, which owns the
//!   macOS / readline editing keys (`⌥⌫`, `⌘⌫`, `Ctrl+W`, `Ctrl+U`,
//!   `Ctrl+A`, …) — which is why visibility and attach don't sit on
//!   `Ctrl+W` / `Ctrl+A`

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::api::endpoints::mime_for_path;
use crate::api::html;
use crate::api::models::{MediaId, StatusId, StatusSource};
use crate::icons;
use crate::state::Visibility;
use crate::ui::Theme;
use crate::ui::widgets::textarea::{TextArea, edit_prompt};

/// Default Mastodon character limit. Phase 3.5 will fetch the real
/// number from `/api/v2/instance`.
pub const DEFAULT_MAX_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct ReplyContext {
    pub id: StatusId,
    pub author_acct: String,
    /// Plain-text excerpt of the parent body, already HTML-decoded.
    pub excerpt: String,
}

/// Target for a native-quote compose. Shape mirrors [`ReplyContext`]
/// so the render path can share truncation logic.
#[derive(Debug, Clone)]
pub struct QuoteContext {
    pub id: StatusId,
    pub author_acct: String,
    pub excerpt: String,
}

/// Most attachments one post can carry (Mastodon's default cap).
pub const DEFAULT_MAX_ATTACHMENTS: usize = 4;

/// What the instance lets one post carry. Both come from
/// `/api/v2/instance.configuration.statuses`; the defaults stand in
/// until that has loaded (or if the server leaves them out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_chars: usize,
    pub max_attachments: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
            max_attachments: DEFAULT_MAX_ATTACHMENTS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachState {
    /// Sent to the state task; waiting for the server to take it.
    Uploading,
    /// Uploaded and processed — safe to reference in the post.
    Ready(MediaId),
}

#[derive(Debug, Clone)]
struct Attachment {
    /// Handle shared with the state task so the answer finds its row.
    local_id: u64,
    name: String,
    alt: Option<String>,
    state: AttachState,
}

/// The one-line prompt `Ctrl+O` opens at the bottom of the screen.
#[derive(Debug, Clone)]
enum Prompt {
    Path(String),
    Alt { path: PathBuf, text: String },
}

/// What the app layer must upload (see `Action::UploadMedia`).
#[derive(Debug, Clone)]
pub struct UploadRequest {
    pub local_id: u64,
    pub path: PathBuf,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Body,
    Spoiler,
}

/// State for the compose screen.
pub struct ComposeState {
    body: TextArea,
    spoiler: TextArea,
    cw_enabled: bool,
    visibility: Visibility,
    reply: Option<ReplyContext>,
    /// Populated by [`ComposeState::quote`]. Mutually exclusive with
    /// `reply` in practice — quoting + replying in the same post isn't
    /// a pattern we support yet.
    quote: Option<QuoteContext>,
    /// Editing an existing post rather than writing a new one.
    edit_of: Option<StatusId>,
    focus: Focus,
    limits: Limits,
    attachments: Vec<Attachment>,
    next_local_id: u64,
    prompt: Option<Prompt>,
    /// One-shot message for the footer ("attachment limit reached (4)"); cleared
    /// by the next key.
    notice: Option<String>,
}

/// Return value of [`ComposeState::handle_key`].
pub enum ComposeOutcome {
    /// Compose continues; UI should re-render but not transition.
    Continue,
    /// User cancelled. The app layer may prompt if body is non-empty
    /// (this method signals intent; it does not confirm).
    Cancel,
    /// User hit submit. Contains the full draft payload.
    Submit(ComposeDraft),
    /// User confirmed an attachment; the app layer should start the
    /// upload.
    Upload(UploadRequest),
}

/// Plain-data form of what we'll send to the API. Translated to
/// `Action::Compose` by the app layer.
pub struct ComposeDraft {
    pub text: String,
    pub in_reply_to_id: Option<StatusId>,
    pub quote_id: Option<StatusId>,
    pub content_warning: Option<String>,
    pub sensitive: bool,
    pub visibility: Visibility,
    pub edit_of: Option<StatusId>,
    /// Uploaded attachments, in the order they were added.
    pub media_ids: Vec<MediaId>,
}

impl ComposeState {
    /// Fresh top-level post. `limits` should come from the instance;
    /// [`Limits::default`] only if it hasn't replied yet.
    #[must_use]
    pub fn blank(limits: Limits) -> Self {
        Self::with(None, None, None, Visibility::Public, limits)
    }

    /// Reply to `reply.id`. Body is pre-filled with `@author ` so the
    /// mention is first-class. Visibility copies the parent's — required
    /// by Mastodon for DM replies to stay DMs.
    #[must_use]
    pub fn reply(reply: ReplyContext, parent_visibility: Visibility, limits: Limits) -> Self {
        let prefix = format!("@{} ", reply.author_acct);
        Self::with(Some(reply), None, Some(prefix), parent_visibility, limits)
    }

    /// Quote `quote.id` (Mastodon 4.5 native quote). Body starts
    /// empty — quote posts typically don't auto-@-mention the
    /// original author; the quote card is the reference. Visibility
    /// defaults to `Public` (user can cycle with Ctrl+V).
    #[must_use]
    pub fn quote(quote: QuoteContext, limits: Limits) -> Self {
        Self::with(None, Some(quote), None, Visibility::Public, limits)
    }

    /// Edit one of the user's own posts. Body and CW are the author's
    /// original text (from `/statuses/:id/source`); visibility can't
    /// be changed by an edit on Mastodon, so the footer shows the
    /// post's current one.
    #[must_use]
    pub fn edit(source: StatusSource, visibility: Visibility, limits: Limits) -> Self {
        let mut me = Self::with(None, None, Some(source.text), visibility, limits);
        if !source.spoiler_text.trim().is_empty() {
            me.cw_enabled = true;
            me.spoiler = TextArea::from_text(source.spoiler_text.trim());
        }
        me.edit_of = Some(source.id);
        me
    }

    fn with(
        reply: Option<ReplyContext>,
        quote: Option<QuoteContext>,
        prefill: Option<String>,
        vis: Visibility,
        limits: Limits,
    ) -> Self {
        let mut body = prefill.map(|s| TextArea::from_text(&s)).unwrap_or_default();
        // Park the cursor at end-of-prefill so typing starts after it.
        body.move_end();
        Self {
            body,
            spoiler: TextArea::new(),
            cw_enabled: false,
            visibility: vis,
            reply,
            quote,
            edit_of: None,
            focus: Focus::Body,
            limits,
            attachments: Vec::new(),
            next_local_id: 0,
            prompt: None,
            notice: None,
        }
    }

    pub fn is_body_empty(&self) -> bool {
        self.body.is_empty()
    }

    fn char_count(&self) -> usize {
        let base = self.body.char_count();
        if self.cw_enabled {
            base + self.spoiler.char_count()
        } else {
            base
        }
    }

    fn over_limit(&self) -> bool {
        self.char_count() > self.limits.max_chars
    }

    fn submit(&self) -> Option<ComposeDraft> {
        let text = self.body.text();
        // Media alone is a valid post; text alone is too. Wait for
        // in-flight uploads — the server rejects half-processed ids.
        let has_media = !self.attachments.is_empty();
        let uploading = self
            .attachments
            .iter()
            .any(|a| a.state == AttachState::Uploading);
        if (text.trim().is_empty() && !has_media) || self.over_limit() || uploading {
            return None;
        }
        let spoiler = if self.cw_enabled {
            let s = self.spoiler.text();
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };
        Some(ComposeDraft {
            text,
            in_reply_to_id: self.reply.as_ref().map(|r| r.id.clone()),
            quote_id: self.quote.as_ref().map(|q| q.id.clone()),
            sensitive: spoiler.is_some(),
            content_warning: spoiler,
            visibility: self.visibility,
            edit_of: self.edit_of.clone(),
            media_ids: self
                .attachments
                .iter()
                .filter_map(|a| match &a.state {
                    AttachState::Ready(id) => Some(id.clone()),
                    AttachState::Uploading => None,
                })
                .collect(),
        })
    }

    /// The state task answered an upload. Returns `false` when the
    /// attachment is gone (the user removed it meanwhile), so the
    /// caller can stay quiet about a failure nobody is waiting on.
    pub fn on_upload_result(&mut self, local_id: u64, result: Result<MediaId, String>) -> bool {
        let Some(pos) = self.attachments.iter().position(|a| a.local_id == local_id) else {
            return false;
        };
        match result {
            Ok(id) => self.attachments[pos].state = AttachState::Ready(id),
            Err(_) => {
                self.attachments.remove(pos);
            }
        }
        true
    }

    /// Keys while the attach prompt is open: it swallows everything.
    fn handle_prompt_key(&mut self, key: KeyEvent) -> ComposeOutcome {
        let Some(prompt) = self.prompt.take() else {
            return ComposeOutcome::Continue;
        };
        match (key.code, prompt) {
            (KeyCode::Esc, _) => {}
            (KeyCode::Enter, Prompt::Path(raw)) => {
                if raw.trim().is_empty() {
                    return ComposeOutcome::Continue;
                }
                let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
                let path = expand_path(&raw, home.as_deref());
                if !path.is_file() {
                    self.notice = Some("no such file".into());
                    self.prompt = Some(Prompt::Path(raw));
                } else if mime_for_path(&path).is_none() {
                    self.notice = Some("unsupported file type".into());
                    self.prompt = Some(Prompt::Path(raw));
                } else {
                    self.prompt = Some(Prompt::Alt {
                        path,
                        text: String::new(),
                    });
                }
            }
            (KeyCode::Enter, Prompt::Alt { path, text }) => {
                let alt = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                let local_id = self.next_local_id;
                self.next_local_id += 1;
                self.attachments.push(Attachment {
                    local_id,
                    name: path
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                    alt: alt.clone(),
                    state: AttachState::Uploading,
                });
                return ComposeOutcome::Upload(UploadRequest {
                    local_id,
                    path,
                    alt,
                });
            }
            (code, mut prompt) => {
                let text = match &mut prompt {
                    Prompt::Path(t) | Prompt::Alt { text: t, .. } => t,
                };
                edit_prompt(text, KeyEvent::new(code, key.modifiers));
                self.prompt = Some(prompt);
            }
        }
        ComposeOutcome::Continue
    }

    fn open_attach_prompt(&mut self) {
        if self.edit_of.is_some() {
            self.notice = Some("can't change attachments while editing".into());
        } else if self.attachments.len() >= self.limits.max_attachments {
            self.notice = Some(format!(
                "attachment limit reached ({})",
                self.limits.max_attachments
            ));
        } else {
            self.prompt = Some(Prompt::Path(String::new()));
        }
    }

    fn cycle_visibility(&mut self) {
        self.visibility = match self.visibility {
            Visibility::Public => Visibility::Unlisted,
            Visibility::Unlisted => Visibility::Private,
            Visibility::Private => Visibility::Direct,
            Visibility::Direct => Visibility::Public,
        };
    }

    fn toggle_cw(&mut self) {
        self.cw_enabled = !self.cw_enabled;
        // Focus always falls back to Body on toggle — never trap the
        // user inside the CW field. If they want to enter CW text,
        // Tab moves focus explicitly.
        if !self.cw_enabled || self.focus == Focus::Spoiler {
            self.focus = Focus::Body;
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Body => {
                if self.cw_enabled {
                    Focus::Spoiler
                } else {
                    Focus::Body
                }
            }
            Focus::Spoiler => Focus::Body,
        };
    }

    /// A bracketed paste. Goes to the open prompt or the focused field
    /// in one go, so pasted newlines never submit anything and the
    /// one-line fields (prompt, CW) get them as spaces.
    pub fn paste(&mut self, text: &str) {
        self.notice = None;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let one_line = || text.trim_end_matches('\n').replace('\n', " ");
        match (&mut self.prompt, self.focus) {
            (Some(Prompt::Path(t) | Prompt::Alt { text: t, .. }), _) => t.push_str(&one_line()),
            (None, Focus::Spoiler) => self.spoiler.insert_str(&one_line()),
            (None, Focus::Body) => self.body.insert_str(&text),
        }
    }

    /// Keyboard driver. See file-level docs for the bindings table.
    pub fn handle_key(&mut self, key: KeyEvent) -> ComposeOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        self.notice = None;
        if self.prompt.is_some() {
            return self.handle_prompt_key(key);
        }

        // Submit: Ctrl+Enter / Alt+Enter / Ctrl+D.
        if matches!(key.code, KeyCode::Enter) && (ctrl || alt) {
            return match self.submit() {
                Some(draft) => ComposeOutcome::Submit(draft),
                None => ComposeOutcome::Continue,
            };
        }
        if matches!(key.code, KeyCode::Char('d')) && ctrl {
            return match self.submit() {
                Some(draft) => ComposeOutcome::Submit(draft),
                None => ComposeOutcome::Continue,
            };
        }
        // Cancel.
        if matches!(key.code, KeyCode::Esc) {
            return ComposeOutcome::Cancel;
        }
        // Modal toggles.
        if ctrl && matches!(key.code, KeyCode::Char('v')) {
            self.cycle_visibility();
            return ComposeOutcome::Continue;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            self.toggle_cw();
            return ComposeOutcome::Continue;
        }
        if ctrl && matches!(key.code, KeyCode::Char('o')) {
            self.open_attach_prompt();
            return ComposeOutcome::Continue;
        }
        if ctrl && matches!(key.code, KeyCode::Char('x')) {
            self.attachments.pop();
            return ComposeOutcome::Continue;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_focus();
            return ComposeOutcome::Continue;
        }

        // CW is semantically single-line — swallow plain Enter so the
        // textarea doesn't scroll previous content off the one-row
        // viewport.
        if self.focus == Focus::Spoiler && matches!(key.code, KeyCode::Enter) {
            return ComposeOutcome::Continue;
        }

        // Route to focused textarea.
        let active = match self.focus {
            Focus::Body => &mut self.body,
            Focus::Spoiler => &mut self.spoiler,
        };
        active.handle_key(key);
        ComposeOutcome::Continue
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, nerd_font: bool) {
        // Vertical stack: title · reply/quote preview · CW field · body · footer.
        let preview_height: u16 = if self.reply.is_some() || self.quote.is_some() {
            3
        } else {
            0
        };
        let cw_height: u16 = if self.cw_enabled { 2 } else { 0 };
        let attach_height = (self.attachments.len() + usize::from(self.prompt.is_some())) as u16;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(2),              // title
                Constraint::Length(preview_height), // reply / quote preview
                Constraint::Length(cw_height),      // CW
                Constraint::Min(3),                 // body
                Constraint::Length(attach_height),  // attachments + prompt
                Constraint::Length(2),              // footer
            ])
            .split(area);

        self.render_title(frame, chunks[0], theme);
        if preview_height > 0 {
            self.render_reference_preview(frame, chunks[1], theme);
        }
        if cw_height > 0 {
            self.render_cw(frame, chunks[2], theme);
        }
        self.render_body(frame, chunks[3], theme);
        if attach_height > 0 {
            self.render_attachments(frame, chunks[4], theme, nerd_font);
        }
        self.render_footer(frame, chunks[5], theme, nerd_font);
    }

    fn render_title(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let label = if self.edit_of.is_some() {
            "Edit"
        } else if self.reply.is_some() {
            "Reply"
        } else if self.quote.is_some() {
            "Quote"
        } else {
            "Compose"
        };
        let count_style = if self.over_limit() {
            theme.error_style()
        } else if self.char_count() * 10 > self.limits.max_chars * 9 {
            theme.favorite_style()
        } else {
            theme.tertiary()
        };
        let count_text = format!("{} / {}", self.char_count(), self.limits.max_chars);
        // Build a line: title on left, counter on right. Padding with
        // spaces is cheaper than a 2-column layout for a single line.
        let pad = area
            .width
            .saturating_sub(label.len() as u16 + count_text.len() as u16);
        let line = Line::from(vec![
            Span::styled(label.to_string(), theme.display_name()),
            Span::raw(" ".repeat(pad as usize)),
            Span::styled(count_text, count_style),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Unified reply / quote preview — identical shape, different
    /// header verb. Reply wins if both are somehow set.
    fn render_reference_preview(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (header, excerpt) = if let Some(r) = &self.reply {
            (
                format!("Replying to @{}", r.author_acct),
                r.excerpt.as_str(),
            )
        } else if let Some(q) = &self.quote {
            (format!("Quoting @{}", q.author_acct), q.excerpt.as_str())
        } else {
            return;
        };
        let excerpt = truncate_chars(excerpt, area.width.saturating_sub(2) as usize, "…");
        let glyph = if self.quote.is_some() { "❝ " } else { "> " };
        let lines = vec![
            Line::from(Span::styled(header, theme.secondary())),
            Line::from(vec![
                Span::styled(glyph.to_string(), theme.tertiary()),
                Span::styled(excerpt, theme.tertiary().add_modifier(Modifier::ITALIC)),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_cw(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        // Label row + one editable row.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        let focused = self.focus == Focus::Spoiler;
        let (label, label_style) = if focused {
            ("● CW", theme.display_name())
        } else {
            ("  CW  (Tab to focus)", theme.tertiary())
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(label, label_style))),
            chunks[0],
        );
        let style = if focused {
            theme.primary()
        } else {
            theme.secondary()
        };
        self.spoiler.render(frame, chunks[1], style, focused);
    }

    fn render_body(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let focused = self.focus == Focus::Body && self.prompt.is_none();
        let style = if focused {
            theme.primary()
        } else {
            theme.secondary()
        };
        self.body.render(frame, area, style, focused);
    }

    /// One row per attachment, then the open prompt (which owns the
    /// cursor while it exists).
    fn render_attachments(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        nerd_font: bool,
    ) {
        let icon = icons::pick(nerd_font, icons::IMAGE, icons::IMAGE_ASCII);
        let mut lines: Vec<Line<'static>> = self
            .attachments
            .iter()
            .map(|a| {
                let (state, state_style) = match a.state {
                    AttachState::Uploading => ("uploading…", theme.favorite_style()),
                    AttachState::Ready(_) => ("ready", theme.boost_style()),
                };
                let alt = a.alt.as_deref().map_or_else(
                    || Span::styled("no alt text", theme.tertiary()),
                    |t| {
                        Span::styled(
                            format!("alt: {}", truncate_chars(t, 40, "…")),
                            theme.secondary(),
                        )
                    },
                );
                Line::from(vec![
                    Span::styled(format!("{icon} "), theme.secondary()),
                    Span::styled(a.name.clone(), theme.primary()),
                    Span::styled("  ·  ", theme.tertiary()),
                    Span::styled(state, state_style),
                    Span::styled("  ·  ", theme.tertiary()),
                    alt,
                ])
            })
            .collect();
        if let Some(prompt) = &self.prompt {
            let (label, text) = match prompt {
                Prompt::Path(t) => ("attach file › ", t),
                Prompt::Alt { text, .. } => ("alt text (Enter to skip) › ", text),
            };
            let width = label.chars().count() + text.chars().count();
            lines.push(Line::from(vec![
                Span::styled(label, theme.display_name()),
                Span::styled(text.clone(), theme.primary()),
            ]));
            let row = (lines.len() - 1) as u16;
            let x = area.x + (width as u16).min(area.width.saturating_sub(1));
            frame.set_cursor_position((x, area.y + row.min(area.height.saturating_sub(1))));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, nerd_font: bool) {
        let vis_icon = if matches!(self.visibility, Visibility::Private | Visibility::Direct) {
            icons::pick(nerd_font, icons::LOCK, icons::LOCK_ASCII)
        } else {
            ""
        };
        let vis_label = match self.visibility {
            Visibility::Public => "public",
            Visibility::Unlisted => "unlisted",
            Visibility::Private => "followers-only",
            Visibility::Direct => "direct message",
        };
        let vis_prefix = if vis_icon.is_empty() {
            String::new()
        } else {
            format!("{vis_icon} ")
        };
        let line1 = Line::from(vec![
            Span::styled("visibility: ", theme.tertiary()),
            Span::styled(
                format!("{vis_prefix}{vis_label}"),
                theme.secondary().add_modifier(Modifier::BOLD),
            ),
        ]);
        let line2 = match &self.notice {
            Some(msg) => Line::from(Span::styled(msg.clone(), theme.error_style())),
            None => Line::from(Span::styled(
                "Ctrl+Enter send · Esc cancel · Ctrl+V vis · Ctrl+S CW · Ctrl+O attach · Tab focus",
                theme.tertiary(),
            )),
        };
        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
    }
}

/// Build a ReplyContext from a Status, capturing the first N chars of
/// plain-text content as the excerpt.
#[must_use]
pub fn reply_context_from(
    status: &crate::api::models::Status,
    excerpt_chars: usize,
) -> ReplyContext {
    let plain = html::to_plain_text(&status.content);
    let excerpt = truncate_chars(&plain, excerpt_chars, "…");
    ReplyContext {
        id: status.id.clone(),
        author_acct: status.account.acct.clone(),
        excerpt,
    }
}

/// Build a QuoteContext from a Status — same shape as ReplyContext but
/// kept as a separate type so mis-wirings at call sites are caught by
/// the type checker.
#[must_use]
pub fn quote_context_from(
    status: &crate::api::models::Status,
    excerpt_chars: usize,
) -> QuoteContext {
    let plain = html::to_plain_text(&status.content);
    let excerpt = truncate_chars(&plain, excerpt_chars, "…");
    QuoteContext {
        id: status.id.clone(),
        author_acct: status.account.acct.clone(),
        excerpt,
    }
}

/// Turn what the user typed or pasted into a path: surrounding quotes
/// and backslash-escaped spaces (what a terminal inserts when a file is
/// dragged onto it) are undone, and a leading `~` becomes `home`.
fn expand_path(raw: &str, home: Option<&Path>) -> PathBuf {
    let t = raw.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(t)
        .replace("\\ ", " ");
    match (t.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ if t == "~" => home.map_or_else(|| PathBuf::from(&t), Path::to_path_buf),
        _ => PathBuf::from(t),
    }
}

fn truncate_chars(s: &str, max: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s
        .chars()
        .take(max.saturating_sub(ellipsis.chars().count()))
        .collect();
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_starts_empty() {
        let c = ComposeState::blank(Limits::default());
        assert!(c.is_body_empty());
        assert_eq!(c.char_count(), 0);
    }

    #[test]
    fn reply_prefills_mention() {
        let r = ReplyContext {
            id: StatusId::new("1"),
            author_acct: "alice@ex.com".into(),
            excerpt: "hi".into(),
        };
        let c = ComposeState::reply(r, Visibility::Public, Limits::default());
        assert_eq!(c.body.text(), "@alice@ex.com ");
    }

    #[test]
    fn ctrl_v_cycles_visibility() {
        let mut c = ComposeState::blank(Limits::default());
        let start = c.visibility;
        c.handle_key(KeyEvent {
            code: KeyCode::Char('v'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_ne!(c.visibility, start);
    }

    #[test]
    fn ctrl_enter_submits_when_non_empty() {
        let mut c = ComposeState::blank(Limits::default());
        c.body = TextArea::from_text("hello");
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        match out {
            ComposeOutcome::Submit(d) => assert_eq!(d.text, "hello"),
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn ctrl_enter_noops_when_empty() {
        let mut c = ComposeState::blank(Limits::default());
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Continue));
    }

    #[test]
    fn quote_starts_empty_and_carries_quote_id() {
        let q = QuoteContext {
            id: StatusId::new("42"),
            author_acct: "bob@ex.com".into(),
            excerpt: "original body".into(),
        };
        let mut c = ComposeState::quote(q, Limits::default());
        assert!(
            c.is_body_empty(),
            "quote compose should start with an empty body"
        );
        c.body = TextArea::from_text("my take");
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        match out {
            ComposeOutcome::Submit(d) => {
                assert_eq!(d.text, "my take");
                assert_eq!(d.quote_id, Some(StatusId::new("42")));
                assert!(d.in_reply_to_id.is_none());
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut c = ComposeState::blank(Limits::default());
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Cancel));
    }

    #[test]
    fn over_limit_blocks_submit() {
        let mut c = ComposeState::blank(Limits::default());
        c.body = TextArea::from_text(&"a".repeat(DEFAULT_MAX_CHARS + 1));
        let out = c.handle_key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(matches!(out, ComposeOutcome::Continue));
    }

    fn press(c: &mut ComposeState, code: KeyCode, mods: KeyModifiers) -> ComposeOutcome {
        c.handle_key(KeyEvent {
            code,
            modifiers: mods,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    fn type_text(c: &mut ComposeState, text: &str) {
        for ch in text.chars() {
            press(c, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// A real (empty) `.png` the attach prompt will accept.
    fn temp_png(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mastoot-test-{}-{name}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    /// Drive the whole attach flow; returns the resulting request.
    fn attach(c: &mut ComposeState, path: &Path, alt: &str) -> UploadRequest {
        press(c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        type_text(c, &path.to_string_lossy());
        press(c, KeyCode::Enter, KeyModifiers::NONE);
        type_text(c, alt);
        match press(c, KeyCode::Enter, KeyModifiers::NONE) {
            ComposeOutcome::Upload(req) => req,
            _ => panic!("expected Upload"),
        }
    }

    #[test]
    fn attach_flow_uploads_then_submits_media_only() {
        let png = temp_png("flow.png");
        let mut c = ComposeState::blank(Limits::default());
        let req = attach(&mut c, &png, "a cat");
        assert_eq!(req.path, png);
        assert_eq!(req.alt.as_deref(), Some("a cat"));

        // Still uploading: Ctrl+Enter must not send half-processed media.
        let submit = |c: &mut ComposeState| press(c, KeyCode::Enter, KeyModifiers::CONTROL);
        assert!(matches!(submit(&mut c), ComposeOutcome::Continue));

        assert!(c.on_upload_result(req.local_id, Ok(MediaId::new("m1"))));
        match submit(&mut c) {
            ComposeOutcome::Submit(d) => {
                assert_eq!(d.media_ids, vec![MediaId::new("m1")]);
                assert_eq!(d.text.trim(), "");
            }
            _ => panic!("media alone should be a valid post"),
        }
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn failed_upload_drops_the_attachment() {
        let png = temp_png("fail.png");
        let mut c = ComposeState::blank(Limits::default());
        let req = attach(&mut c, &png, "");
        assert_eq!(req.alt, None, "an empty alt prompt means no alt text");
        assert!(c.on_upload_result(req.local_id, Err("boom".into())));
        assert!(c.attachments.is_empty());
        // The row is gone, so a late answer is not news.
        assert!(!c.on_upload_result(req.local_id, Ok(MediaId::new("m"))));
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn bad_paths_keep_the_prompt_open() {
        let mut c = ComposeState::blank(Limits::default());
        press(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        type_text(&mut c, "/definitely/not/here.png");
        press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(c.prompt, Some(Prompt::Path(_))));
        assert_eq!(c.notice.as_deref(), Some("no such file"));

        // Esc closes the prompt without cancelling the compose.
        let out = press(&mut c, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(out, ComposeOutcome::Continue));
        assert!(c.prompt.is_none());
    }

    #[test]
    fn ctrl_x_removes_the_last_attachment_and_edit_mode_refuses() {
        let png = temp_png("rm.png");
        let mut c = ComposeState::blank(Limits::default());
        attach(&mut c, &png, "");
        press(&mut c, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(c.attachments.is_empty());

        let mut e = ComposeState::blank(Limits::default());
        e.edit_of = Some(StatusId::new("9"));
        press(&mut e, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(e.prompt.is_none());
        assert!(e.notice.is_some());
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn paste_lands_in_the_focused_field_without_submitting() {
        let mut c = ComposeState::blank(Limits::default());
        c.paste("line one\r\nline two");
        assert_eq!(c.body.text(), "line one\nline two");

        c.cw_enabled = true;
        c.focus = Focus::Spoiler;
        c.paste("spoilers\nahead\n");
        assert_eq!(c.spoiler.text(), "spoilers ahead");

        // A file dragged onto the terminal pastes its escaped path.
        press(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        c.paste("/tmp/my\\ cat.png\n");
        assert!(matches!(&c.prompt, Some(Prompt::Path(p)) if p == "/tmp/my\\ cat.png"));
    }

    #[test]
    fn attachment_cap_follows_the_instance() {
        let png = temp_png("cap.png");
        let mut c = ComposeState::blank(Limits {
            max_attachments: 1,
            ..Limits::default()
        });
        attach(&mut c, &png, "");
        press(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(c.prompt.is_none(), "a second file must be refused");
        assert_eq!(c.notice.as_deref(), Some("attachment limit reached (1)"));
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn expand_path_undoes_shell_escaping_and_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(
            expand_path("~/Pictures/a b.png", Some(home)),
            PathBuf::from("/home/me/Pictures/a b.png")
        );
        assert_eq!(
            expand_path("/tmp/a\\ b.png", Some(home)),
            PathBuf::from("/tmp/a b.png")
        );
        assert_eq!(
            expand_path("'/tmp/x y.png'", Some(home)),
            PathBuf::from("/tmp/x y.png")
        );
        assert_eq!(
            expand_path("\"/tmp/q.png\"", None),
            PathBuf::from("/tmp/q.png")
        );
    }

    fn screen(c: &mut ComposeState) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (w, h) = (72u16, 14u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            c.render(f, area, &Theme::frost(), false);
        })
        .unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                let row: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn attachment_rows_and_prompt_render_above_the_footer() {
        let png = temp_png("render.png");
        let mut c = ComposeState::blank(Limits::default());
        type_text(&mut c, "hello");
        let req = attach(&mut c, &png, "a cat");
        c.on_upload_result(req.local_id, Ok(MediaId::new("m1")));
        press(&mut c, KeyCode::Char('o'), KeyModifiers::CONTROL);
        type_text(&mut c, "~/pic");

        let name = png.file_name().unwrap().to_string_lossy().into_owned();
        let out = screen(&mut c);
        let lines: Vec<&str> = out.lines().collect();
        let row = |needle: &str| lines.iter().position(|l| l.contains(needle));
        let att = row(&format!("[img] {name}  ·  ready  ·  alt: a cat")).expect(&out);
        let prompt = row("attach file › ~/pic").expect(&out);
        let footer = row("visibility:").expect(&out);
        assert!(att < prompt && prompt < footer, "{out}");
        assert!(row("Ctrl+O attach").is_some(), "{out}");
        let _ = std::fs::remove_file(png);
    }
}
