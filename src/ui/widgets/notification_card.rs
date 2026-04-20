//! One notification → a flat list of visual lines, gutter-aligned.
//!
//! Layout pattern, parallel to [`crate::ui::widgets::status_card`]:
//!
//! - **Action line** with a typed icon and a one-sentence description
//!   (`󰓎 @alice favourited your post · 2h`).
//! - For mention / reblog / favourite / quote / poll / status / update,
//!   a dim excerpt of the linked status (first ~3 wrapped lines).
//! - Follow / follow-request / admin types render only the action line.
//!
//! No metric counts here — the linked status detail page is the place
//! to expand. Notifications stay scannable.

use chrono::Utc;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::api::html;
use crate::api::models::{Notification, NotificationType};
use crate::icons;
use crate::ui::Theme;
use crate::ui::widgets::wrap;
use crate::util::time::relative;

/// Render a notification as already-wrapped visual lines, including
/// the same 2-column gutter as status cards (cursor bar or two spaces).
#[must_use]
pub fn render(
    n: &Notification,
    theme: &Theme,
    selected: bool,
    nerd_font: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let body = build_lines(n, theme, nerd_font, width.saturating_sub(2));
    body.into_iter()
        .map(|l| with_gutter(l, theme, selected))
        .collect()
}

fn build_lines(
    n: &Notification,
    theme: &Theme,
    nerd_font: bool,
    inner_width: u16,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    let display = if n.account.display_name.is_empty() {
        n.account.username.clone()
    } else {
        n.account.display_name.clone()
    };
    let handle = format!("@{}", n.account.acct);
    let time = n
        .created_at
        .map(|ts| relative(Utc::now(), ts))
        .unwrap_or_default();

    let (icon, verb, icon_style) = describe(n.notification_type, nerd_font, theme);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(display, theme.display_name()),
        Span::raw(" "),
        Span::styled(verb, theme.secondary()),
        Span::raw("  "),
        Span::styled(handle, theme.handle()),
    ];
    if !time.is_empty() {
        spans.push(Span::styled("  ·  ", theme.timestamp()));
        spans.push(Span::styled(time, theme.timestamp()));
    }
    out.push(Line::from(spans));

    // Optional status excerpt (first ~3 wrapped lines, dim).
    if let Some(status) = n.status.as_ref() {
        let body = html::render(&status.content, theme);
        // Re-flow at inner_width minus a 2-col indent so the excerpt
        // visibly nests under the action line.
        let excerpt_w = inner_width.saturating_sub(2);
        let wrapped = wrap::wrap_lines(&body, excerpt_w);
        for (i, mut line) in wrapped.into_iter().enumerate() {
            if i >= EXCERPT_MAX_LINES {
                let truncated_marker = Line::from(Span::styled("  …", theme.tertiary()));
                out.push(truncated_marker);
                break;
            }
            // Dim the whole excerpt and indent.
            for span in &mut line.spans {
                span.style = span.style.patch(theme.tertiary());
            }
            line.spans.insert(0, Span::raw("  "));
            out.push(line);
        }
    }

    out
}

const EXCERPT_MAX_LINES: usize = 3;

fn describe(
    kind: NotificationType,
    nerd_font: bool,
    theme: &Theme,
) -> (&'static str, &'static str, Style) {
    match kind {
        NotificationType::Mention => (
            icons::pick(nerd_font, icons::REPLY, icons::REPLY_ASCII),
            "mentioned you",
            theme.secondary(),
        ),
        NotificationType::Reblog => (
            icons::pick(nerd_font, icons::BOOST, icons::BOOST_ASCII),
            "boosted your post",
            theme.boost_style(),
        ),
        NotificationType::Favourite => (
            icons::pick(nerd_font, icons::FAVORITE, icons::FAVORITE_ASCII),
            "favourited your post",
            theme.favorite_style(),
        ),
        NotificationType::Follow => (
            icons::pick(nerd_font, icons::NOTIFICATION, icons::NOTIFICATION_ASCII),
            "followed you",
            theme.link(),
        ),
        NotificationType::FollowRequest => (
            icons::pick(nerd_font, icons::NOTIFICATION, icons::NOTIFICATION_ASCII),
            "wants to follow you",
            theme.link(),
        ),
        NotificationType::Poll => (
            icons::pick(nerd_font, icons::NOTIFICATION, icons::NOTIFICATION_ASCII),
            "poll ended",
            theme.secondary(),
        ),
        NotificationType::Status => (
            icons::pick(nerd_font, icons::REPLY, icons::REPLY_ASCII),
            "posted",
            theme.secondary(),
        ),
        NotificationType::Update => (
            icons::pick(nerd_font, icons::WARNING, icons::WARNING_ASCII),
            "edited a post",
            theme.secondary(),
        ),
        NotificationType::Quote => (
            icons::pick(nerd_font, icons::REPLY, icons::REPLY_ASCII),
            "quoted you",
            theme.secondary(),
        ),
        NotificationType::QuotedUpdate => (
            icons::pick(nerd_font, icons::WARNING, icons::WARNING_ASCII),
            "edited a quote of yours",
            theme.secondary(),
        ),
        NotificationType::SeveredRelationships => (
            icons::pick(nerd_font, icons::WARNING, icons::WARNING_ASCII),
            "severed relationships",
            theme.error_style(),
        ),
        NotificationType::ModerationWarning => (
            icons::pick(nerd_font, icons::WARNING, icons::WARNING_ASCII),
            "moderation warning",
            theme.error_style(),
        ),
        NotificationType::AdminSignUp => (
            icons::pick(nerd_font, icons::NOTIFICATION, icons::NOTIFICATION_ASCII),
            "signed up",
            theme.secondary(),
        ),
        NotificationType::AdminReport => (
            icons::pick(nerd_font, icons::WARNING, icons::WARNING_ASCII),
            "filed a report",
            theme.error_style(),
        ),
        NotificationType::Other => (
            icons::pick(nerd_font, icons::NOTIFICATION, icons::NOTIFICATION_ASCII),
            "notification",
            theme.secondary(),
        ),
    }
}

fn with_gutter(line: Line<'static>, theme: &Theme, selected: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    if selected {
        spans.push(Span::styled(format!("{} ", icons::CURSOR), theme.cursor()));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.extend(line.spans);
    Line::from(spans)
}

/// One blank row between adjacent notifications. Slightly tighter
/// than between status cards because a notification is shorter and
/// reads more like a list item than a paragraph.
pub const INTER_NOTIFICATION_BLANK_LINES: usize = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{Account, NotificationId};

    fn fake_notif(kind: NotificationType) -> Notification {
        Notification {
            id: NotificationId::new("n1"),
            notification_type: kind,
            created_at: Some(Utc::now() - chrono::Duration::hours(2)),
            account: Account {
                acct: "alice@ex.com".into(),
                display_name: "Alice".into(),
                ..Default::default()
            },
            status: None,
            report: None,
        }
    }

    fn join(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect()
    }

    #[test]
    fn favourite_renders_action_text() {
        let theme = Theme::frost();
        let n = fake_notif(NotificationType::Favourite);
        let lines = render(&n, &theme, false, true, 80);
        let text = join(&lines);
        assert!(text.contains("favourited your post"));
        assert!(text.contains("@alice@ex.com"));
    }

    #[test]
    fn follow_has_no_status_excerpt() {
        let theme = Theme::frost();
        let n = fake_notif(NotificationType::Follow);
        let lines = render(&n, &theme, false, true, 80);
        assert_eq!(lines.len(), 1, "follow should emit just the action line");
    }

    #[test]
    fn long_excerpt_truncates() {
        let theme = Theme::frost();
        let mut n = fake_notif(NotificationType::Mention);
        let body = (0..20)
            .map(|i| format!("line {i} blah blah blah blah blah blah blah blah blah blah"))
            .collect::<Vec<_>>()
            .join(" ");
        n.status = Some(crate::api::models::Status {
            content: format!("<p>{body}</p>"),
            ..Default::default()
        });
        let lines = render(&n, &theme, false, true, 40);
        // 1 action + ≤ EXCERPT_MAX_LINES + at most 1 ellipsis line.
        assert!(lines.len() <= 1 + EXCERPT_MAX_LINES + 1);
        assert!(join(&lines).contains('…'));
    }

    /// Make sure the gutter math doesn't truncate a CJK character.
    #[test]
    fn gutter_keeps_visible_width_consistent() {
        use unicode_width::UnicodeWidthChar;
        let theme = Theme::frost();
        let n = fake_notif(NotificationType::Favourite);
        let lines = render(&n, &theme, true, true, 80);
        let first = lines.first().unwrap();
        // Gutter is two columns: cursor glyph (▏ = 1 col) + space.
        let prefix: String = first.spans[0].content.chars().collect();
        let cols: usize = prefix
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        assert_eq!(cols, 2);
    }
}
