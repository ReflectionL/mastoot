//! Golden-screen tests: render whole screens into a
//! [`ratatui::backend::TestBackend`] and compare the text grid.
//!
//! These catch the class of bug a unit test on one card can't — a
//! clipped last row, a missing blank line between posts, an indent
//! that drifted — without a terminal or a Mastodon account. Fixtures
//! avoid anything time-dependent (`created_at` is `None`, so no
//! relative timestamps) and use ASCII icons so the expectation reads
//! as plain text.
//!
//! When a change is intentional, update the golden string; the
//! failure message prints the actual grid ready to paste.

use std::collections::HashMap;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::api::models::{Account, AccountId, SearchResults, Status, StatusId};
use crate::api::music::MusicCache;
use crate::state::TimelineKind;
use crate::ui::Theme;
use crate::ui::images::ImageCache;
use crate::ui::screens::search::SearchScreen;
use crate::ui::screens::status_detail::DetailState;
use crate::ui::screens::timeline::TimelineScreen;
use crate::ui::widgets::status_card::RenderPrefs;

fn prefs() -> RenderPrefs {
    RenderPrefs {
        nerd_font: false,
        absolute_time: false,
        inter_post_blank_lines: 1,
    }
}

fn account(acct: &str, name: &str) -> Account {
    Account {
        id: AccountId::new(format!("id-{acct}")),
        username: acct.split('@').next().unwrap_or(acct).to_string(),
        acct: acct.to_string(),
        display_name: name.to_string(),
        ..Default::default()
    }
}

fn post(id: &str, acct: &str, name: &str, html: &str) -> Status {
    Status {
        id: StatusId::new(id),
        account: account(acct, name),
        content: html.to_string(),
        ..Default::default()
    }
}

/// Render with `draw` into a `w`×`h` grid and return the rows, each
/// right-trimmed, joined by newlines.
fn grid(w: u16, h: u16, mut draw: impl FnMut(&mut ratatui::Frame<'_>, Rect)) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        let area = f.area();
        draw(f, area);
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

fn assert_grid(actual: &str, expected: &str) {
    let expected = expected.trim_matches('\n');
    let actual = actual.trim_matches('\n');
    assert!(
        actual == expected,
        "screen differs.\n--- expected ---\n{expected}\n--- actual ---\n{actual}\n"
    );
}

#[test]
fn timeline_cards_reply_preview_and_boost() {
    let theme = Theme::frost();
    let mut screen = TimelineScreen::new(TimelineKind::Home);
    let parent = post(
        "p",
        "bob@ex.com",
        "Bob",
        "<p>What about the other thing?</p>",
    );
    let mut reply = post("r", "alice@ex.com", "Alice", "<p>Yes, that too.</p>");
    reply.in_reply_to_id = Some(StatusId::new("p"));
    reply.in_reply_to_account_id = Some(AccountId::new("id-bob@ex.com"));
    let mut boost = post("b", "carol@ex.com", "Carol", "");
    boost.reblog = Some(Box::new(post(
        "orig",
        "dave@ex.com",
        "Dave",
        "<p>First paragraph.</p><p>Second paragraph.</p>",
    )));
    let items = vec![reply, parent, boost];
    screen.on_items_changed(items.len(), false);
    let parents: HashMap<StatusId, Status> = HashMap::new();
    let mut music = MusicCache::new();
    let mut images = ImageCache::disabled();
    let actual = grid(50, 16, |f, area| {
        screen.render(
            f,
            area,
            &items,
            &theme,
            prefs(),
            &parents,
            &mut music,
            &mut images,
        );
    });
    assert_grid(
        &actual,
        r"
 ▏ Alice  @alice@ex.com
 ▏ ↪ @bob@ex.com: “What about the other thing?”
 ▏ Yes, that too.

   Bob  @bob@ex.com
   What about the other thing?

   [boost] @carol@ex.com boosted
   Dave  @dave@ex.com
   First paragraph.

   Second paragraph.
",
    );
}

#[test]
fn thread_nests_replies_by_depth() {
    let theme = Theme::frost();
    let mut detail = DetailState::new(post("focal", "op@ex.com", "OP", "<p>Question?</p>"));
    let mut r1 = post("r1", "a@ex.com", "A", "<p>Answer one.</p>");
    r1.in_reply_to_id = Some(StatusId::new("focal"));
    let mut r1a = post("r1a", "b@ex.com", "B", "<p>Follow-up to one.</p>");
    r1a.in_reply_to_id = Some(StatusId::new("r1"));
    let mut r2 = post("r2", "c@ex.com", "C", "<p>Answer two.</p>");
    r2.in_reply_to_id = Some(StatusId::new("focal"));
    detail.on_context_loaded(vec![], vec![r1, r1a, r2]);
    let mut music = MusicCache::new();
    let mut images = ImageCache::disabled();
    let actual = grid(44, 18, |f, area| {
        detail.render(f, area, &theme, prefs(), &mut images, &mut music);
    });
    assert_grid(
        &actual,
        r"
 ▏ OP  @op@ex.com
 ▏ Question?
 ▏
 ▏ ↪ 0   [boost] 0   * 0

   A  @a@ex.com
   Answer one.

     B  @b@ex.com
     Follow-up to one.

   C  @c@ex.com
   Answer two.
",
    );
}

#[test]
fn search_results_group_into_sections() {
    let theme = Theme::frost();
    let mut screen = SearchScreen::new("ex".into());
    screen.on_results(SearchResults {
        accounts: vec![account("erin@ex.com", "Erin")],
        statuses: vec![post("s", "frank@ex.com", "Frank", "<p>Example post.</p>")],
        hashtags: vec![],
    });
    let mut music = MusicCache::new();
    let mut images = ImageCache::disabled();
    let actual = grid(44, 12, |f, area| {
        screen.render(f, area, &theme, prefs(), &mut music, &mut images);
    });
    assert_grid(
        &actual,
        r"
 accounts

 ▏ Erin  @erin@ex.com

 posts

   Frank  @frank@ex.com
   Example post.
",
    );
}

#[test]
fn selected_card_at_the_bottom_is_fully_visible() {
    // Regression for the scroll clamp that ignored the Block's top
    // padding: the last card's final line must be on screen.
    let theme = Theme::frost();
    let mut screen = TimelineScreen::new(TimelineKind::Home);
    let items: Vec<Status> = (0..6)
        .map(|i| {
            post(
                &format!("s{i}"),
                "u@ex.com",
                "U",
                &format!("<p>post number {i}</p><p>second line</p>"),
            )
        })
        .collect();
    screen.on_items_changed(items.len(), false);
    screen.selected = 5;
    let parents: HashMap<StatusId, Status> = HashMap::new();
    let mut music = MusicCache::new();
    let mut images = ImageCache::disabled();
    let actual = grid(40, 9, |f, area| {
        screen.render(
            f,
            area,
            &items,
            &theme,
            prefs(),
            &parents,
            &mut music,
            &mut images,
        );
    });
    let last = actual.lines().last().unwrap_or("");
    assert_eq!(last, " ▏ second line", "\n{actual}");
}
