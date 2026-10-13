//! Keyboard handling: global keys, per-mode dispatch, and the
//! selection / navigation helpers they share.

use super::{
    Account, AccountListKind, AccountListOutcome, AccountListScreen, AccountRef,
    AccountSwitcherScreen, Action, App, ComposeOutcome, ComposeState, DetailOutcome, DetailState,
    KeyCode, KeyEvent, KeyModifiers, Mode, NotifOutcome, ProfileOutcome, ProfileScreen, Result,
    SearchOutcome, SearchScreen, Status, StatusId, SwitcherOutcome, TimelineKind, ToastLevel,
    api_to_state_vis, config, quote_context_from, reply_context_from, state,
};

impl App {
    /// Handle a key press. Returns `false` when the app should quit.
    pub(super) async fn handle_key(
        &mut self,
        key: KeyEvent,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) -> bool {
        // Ctrl-C always quits (belt-and-suspenders: raw mode swallows
        // SIGINT, so we must handle it at the key layer).
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        // Splash swallows the rest — nothing to navigate until the
        // first timeline or health signal lands.
        if self.splash {
            return true;
        }

        // Any key dismisses the help overlay.
        if self.show_help {
            self.show_help = false;
            return true;
        }

        // Density toggle works everywhere — `D` flips inter-post
        // blank-line count between 1 and 2 so the user can A/B
        // information density vs breathing room without restarting.
        // Filtered out of compose mode below so it doesn't eat a
        // literal `D` key in the body editor.
        if key.code == KeyCode::Char('D')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
        {
            self.density = if self.density >= 2 { 1 } else { 2 };
            return true;
        }

        // Live-update mode cycle: streaming → polling → off → streaming.
        // Like `D`, the `S` key works everywhere *except* compose so it
        // doesn't eat a literal S in the body.
        if key.code == KeyCode::Char('S')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
        {
            let next = self.stream_mode.cycle();
            self.stream_mode = next;
            let _ = tx.send(Action::SetStreamMode(next)).await;
            return true;
        }

        // `/` opens the search prompt from any browsing mode.
        if key.code == KeyCode::Char('/')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                self.mode,
                Mode::Timeline
                    | Mode::StatusDetail(_)
                    | Mode::Profile(_)
                    | Mode::AccountList(_)
                    | Mode::Search(_)
            )
        {
            self.push_mode(Mode::SearchPrompt(String::new()));
            return true;
        }

        // `A` opens the account switcher. A second `A` or `Esc` closes
        // it. Unlike the tab keys, this is a *modal* — we push the
        // current mode onto the back-stack so exiting without picking
        // leaves the user where they were.
        if key.code == KeyCode::Char('A')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !matches!(
                self.mode,
                Mode::Compose(_) | Mode::ComposeConfirmDiscard(_) | Mode::AccountSwitcher(_)
            )
        {
            let accounts = self.cfg.accounts.clone();
            let current = self.cfg.default_account.clone();
            self.push_mode(Mode::AccountSwitcher(AccountSwitcherScreen::new(
                accounts, current,
            )));
            return true;
        }

        // Keys that act on "the selected status" behave identically in
        // timeline / thread / profile, so they're handled once here
        // instead of per mode below.
        if self.handle_status_keys(key, tx).await {
            return true;
        }

        match std::mem::replace(&mut self.mode, Mode::Timeline) {
            Mode::SearchPrompt(mut query) => {
                match key.code {
                    KeyCode::Esc => self.pop_mode(),
                    KeyCode::Enter => {
                        let q = query.trim().to_string();
                        if q.is_empty() {
                            self.pop_mode();
                        } else {
                            // Replace the prompt (not push): the prompt
                            // is transient, `h` from results should
                            // return to where `/` was pressed.
                            self.mode = Mode::Search(SearchScreen::new(q.clone()));
                            let _ = tx.send(Action::Search { query: q }).await;
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.mode = Mode::SearchPrompt(query);
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.mode = Mode::SearchPrompt(String::new());
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        self.mode = Mode::SearchPrompt(query);
                    }
                    _ => self.mode = Mode::SearchPrompt(query),
                }
                return true;
            }
            Mode::Search(mut state) => {
                if let KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') = key.code {
                    // Tab switch — fall through to timeline keys.
                    self.mode = Mode::Timeline;
                } else {
                    match state.handle_key(key) {
                        SearchOutcome::Continue => self.mode = Mode::Search(state),
                        SearchOutcome::Back => self.pop_mode(),
                        SearchOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                            self.mode = Mode::Search(state);
                        }
                        SearchOutcome::OpenProfile(acc) => {
                            self.mode = Mode::Search(state);
                            self.open_profile(acc, tx).await;
                        }
                        SearchOutcome::OpenStatus(s) => {
                            self.mode = Mode::Search(state);
                            self.open_detail(s, tx).await;
                        }
                        SearchOutcome::SearchTag(name) => {
                            self.back_stack.push(Mode::Search(state));
                            self.mode = Mode::Search(SearchScreen::new(format!("#{name}")));
                            let _ = tx.send(Action::SearchTag { name }).await;
                        }
                    }
                    return true;
                }
                // Fall through for tab keys.
            }
            Mode::QuitConfirm => {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => return false,
                    KeyCode::Esc | KeyCode::Char('n' | 'N' | 'h') | KeyCode::Backspace => {
                        self.pop_mode();
                    }
                    _ => {
                        self.mode = Mode::QuitConfirm;
                    }
                }
                return true;
            }
            Mode::Compose(mut state) => {
                match state.handle_key(key) {
                    ComposeOutcome::Continue => {
                        self.mode = Mode::Compose(state);
                    }
                    ComposeOutcome::Cancel => {
                        if state.is_body_empty() {
                            // nothing to lose — bounce straight back to
                            // wherever we came from.
                            self.exit_compose(tx, false).await;
                        } else {
                            self.mode = Mode::ComposeConfirmDiscard(state);
                        }
                    }
                    ComposeOutcome::Submit(draft) => {
                        let was_reply = draft.in_reply_to_id.is_some();
                        let _ = tx
                            .send(Action::Compose {
                                text: draft.text,
                                in_reply_to_id: draft.in_reply_to_id,
                                quote_id: draft.quote_id,
                                content_warning: draft.content_warning,
                                sensitive: draft.sensitive,
                                visibility: draft.visibility,
                                edit_of: draft.edit_of,
                            })
                            .await;
                        // If we came from a detail page and this was a
                        // reply, kick a context reload so the new post
                        // shows up under the focal.
                        let kick = was_reply
                            && matches!(self.back_stack.last(), Some(Mode::StatusDetail(_)));
                        self.exit_compose(tx, kick).await;
                        self.push_toast(ToastLevel::Info, "posting…".into());
                    }
                }
                return true;
            }
            Mode::ComposeConfirmDiscard(state) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        // discard — drop state, back to wherever.
                        self.exit_compose(tx, false).await;
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        // keep editing.
                        self.mode = Mode::Compose(state);
                    }
                    _ => {
                        self.mode = Mode::ComposeConfirmDiscard(state);
                    }
                }
                return true;
            }
            Mode::DeleteConfirm(id) => {
                match key.code {
                    KeyCode::Enter => {
                        let _ = tx.send(Action::DeleteStatus(id)).await;
                        self.pop_mode();
                    }
                    KeyCode::Esc | KeyCode::Char('h') | KeyCode::Backspace => {
                        self.pop_mode();
                    }
                    _ => {
                        self.mode = Mode::DeleteConfirm(id);
                    }
                }
                return true;
            }
            Mode::Profile(mut state) => {
                // App-level intercept: tab keys + `u` (re-open another
                // profile) before delegating.
                match key.code {
                    KeyCode::Char('5') if state.is_self => {
                        // Already on self-profile — keep state, no-op.
                        self.mode = Mode::Profile(state);
                        return true;
                    }
                    KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') => {
                        // Restore mode to Timeline so the tab handlers
                        // below see the right starting state, then let
                        // the timeline-mode key table run.
                        self.mode = Mode::Timeline;
                        // fall through to timeline keys
                    }
                    KeyCode::Char('o' | 'O') => {
                        let kind = if matches!(key.code, KeyCode::Char('O')) {
                            AccountListKind::Following
                        } else {
                            AccountListKind::Followers
                        };
                        let id = state.account_id.clone();
                        let handle = state
                            .account
                            .as_ref()
                            .map_or_else(|| format!("@{id}"), |a| format!("@{}", a.acct));
                        self.back_stack.push(Mode::Profile(state));
                        self.mode =
                            Mode::AccountList(AccountListScreen::new(id.clone(), handle, kind));
                        let _ = tx
                            .send(Action::LoadAccountList {
                                id,
                                kind,
                                max_id: None,
                            })
                            .await;
                        return true;
                    }
                    _ => {
                        match state.handle_key(key) {
                            ProfileOutcome::Continue => {
                                self.mode = Mode::Profile(state);
                            }
                            ProfileOutcome::Back => {
                                self.pop_mode();
                            }
                            ProfileOutcome::Dispatch(a) => {
                                let _ = tx.send(a).await;
                                self.mode = Mode::Profile(state);
                            }
                            ProfileOutcome::OpenStatus(s) => {
                                let detail = DetailState::new(s);
                                let id = detail.focal_id().clone();
                                // Push the profile so `h` from detail
                                // returns here.
                                self.back_stack.push(Mode::Profile(state));
                                self.mode = Mode::StatusDetail(detail);
                                let _ = tx.send(Action::OpenStatus(id)).await;
                            }
                        }
                        return true;
                    }
                }
                // Fall through to timeline keys (tab switch).
            }
            Mode::AccountList(mut state) => {
                if let KeyCode::Char('1' | '2' | '3' | '4' | '5' | '6' | '7') = key.code {
                    // Tab switch — fall through to timeline keys.
                    self.mode = Mode::Timeline;
                } else {
                    match state.handle_key(key) {
                        AccountListOutcome::Continue => {
                            self.mode = Mode::AccountList(state);
                        }
                        AccountListOutcome::Back => {
                            self.pop_mode();
                        }
                        AccountListOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                            self.mode = Mode::AccountList(state);
                        }
                        AccountListOutcome::OpenProfile(acc) => {
                            let id = acc.id.clone();
                            self.back_stack.push(Mode::AccountList(state));
                            self.mode = Mode::Profile(ProfileScreen::new(acc, false));
                            let _ = tx
                                .send(Action::LoadProfile {
                                    id: id.clone(),
                                    max_id: None,
                                })
                                .await;
                            let _ = tx.send(Action::LoadRelationship(id)).await;
                        }
                    }
                    return true;
                }
                // Fall through for tab keys.
            }
            Mode::AccountSwitcher(mut state) => {
                match state.handle_key(key) {
                    SwitcherOutcome::Continue => {
                        self.mode = Mode::AccountSwitcher(state);
                    }
                    SwitcherOutcome::Back => {
                        self.pop_mode();
                    }
                    SwitcherOutcome::Pick(acc) => {
                        if Some(acc.handle.as_str()) == self.cfg.default_account.as_deref() {
                            // Picking the already-current account —
                            // just close, no churn.
                            self.pop_mode();
                        } else {
                            match self.begin_account_switch(&acc, tx).await {
                                Ok(()) => {
                                    // Clear back-stack; after a switch
                                    // the previous mode's cached state
                                    // refers to the old session's data.
                                    self.back_stack.clear();
                                    self.mode = Mode::Timeline;
                                }
                                Err(msg) => {
                                    self.push_toast(ToastLevel::Error, msg);
                                    self.mode = Mode::AccountSwitcher(state);
                                }
                            }
                        }
                    }
                }
                return true;
            }
            Mode::StatusDetail(mut state) => {
                match state.handle_key(key) {
                    DetailOutcome::Continue => {
                        self.mode = Mode::StatusDetail(state);
                    }
                    DetailOutcome::Back => {
                        self.pop_mode();
                    }
                    DetailOutcome::Dispatch(action) => {
                        let _ = tx.send(action).await;
                        self.mode = Mode::StatusDetail(state);
                    }
                }
                return true;
            }
            Mode::Timeline => {
                // fall through into the timeline key table below
            }
        }

        // Timeline mode keys.
        match key.code {
            KeyCode::Esc => {
                self.push_mode(Mode::QuitConfirm);
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('1') => self.switch_to(TimelineKind::Home, tx).await,
            KeyCode::Char('2') => self.switch_to(TimelineKind::Local, tx).await,
            KeyCode::Char('3') => self.switch_to(TimelineKind::Federated, tx).await,
            KeyCode::Char('4') => self.switch_to(TimelineKind::Notifications, tx).await,
            KeyCode::Char('5') => self.open_self_profile(tx).await,
            KeyCode::Char('6') => self.switch_to(TimelineKind::Favourites, tx).await,
            KeyCode::Char('7') => self.switch_to(TimelineKind::Bookmarks, tx).await,
            KeyCode::Char('f') => {
                if let Some(action) = self.toggle_favourite_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('b') => {
                if let Some(action) = self.toggle_reblog_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('B') => {
                if let Some(action) = self.force_unreblog_optimistic() {
                    let _ = tx.send(action).await;
                }
            }
            KeyCode::Char('l') | KeyCode::Enter => {
                if let Some(detail) = self.open_detail_for_selection() {
                    let id = detail.focal_id().clone();
                    self.push_mode(Mode::StatusDetail(detail));
                    let _ = tx.send(Action::OpenStatus(id)).await;
                }
            }
            _ => {
                // Delegate to the active screen.
                let kind = self.active;
                if kind == TimelineKind::Notifications {
                    let outcome = self
                        .notifications_screen
                        .handle_key(key, &self.notifications);
                    match outcome {
                        NotifOutcome::Continue => {}
                        NotifOutcome::Dispatch(a) => {
                            let _ = tx.send(a).await;
                        }
                        NotifOutcome::OpenStatus(status) => {
                            let detail = DetailState::new(status);
                            let id = detail.focal_id().clone();
                            self.push_mode(Mode::StatusDetail(detail));
                            let _ = tx.send(Action::OpenStatus(id)).await;
                        }
                    }
                } else {
                    let empty: Vec<Status> = Vec::new();
                    let items = self.timelines.get(&kind).unwrap_or(&empty);
                    let screen = self.screens.get_mut(&kind).expect("screen initialized");
                    if let Some(action) = screen.handle_key(key, items) {
                        let _ = tx.send(action).await;
                    }
                }
            }
        }
        true
    }

    /// Status-scoped keys shared by every mode that has a selected
    /// post (timeline incl. notifications, thread, profile). Returns
    /// `true` when the key was consumed. Keys that only make sense in
    /// one place (`o` = followers inside a profile) are left to that
    /// mode's own table.
    pub(super) async fn handle_status_keys(
        &mut self,
        key: KeyEvent,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if !matches!(
            self.mode,
            Mode::Timeline | Mode::StatusDetail(_) | Mode::Profile(_) | Mode::Search(_)
        ) {
            return false;
        }
        let in_profile = matches!(self.mode, Mode::Profile(_));
        match key.code {
            KeyCode::Char('c') => {
                self.push_mode(Mode::Compose(ComposeState::blank(self.max_chars)));
            }
            KeyCode::Char('r') => {
                if let Some(target) = self.current_target().cloned() {
                    let reply = reply_context_from(&target, 80);
                    let vis = api_to_state_vis(target.visibility);
                    self.push_mode(Mode::Compose(ComposeState::reply(
                        reply,
                        vis,
                        self.max_chars,
                    )));
                }
            }
            KeyCode::Char('q') => {
                if let Some(target) = self.current_target().cloned() {
                    let quote = quote_context_from(&target, 80);
                    self.push_mode(Mode::Compose(ComposeState::quote(quote, self.max_chars)));
                }
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_own_status_id() {
                    self.push_mode(Mode::DeleteConfirm(id));
                }
            }
            KeyCode::Char('e') => {
                // Edit own post: fetch the original text first; the
                // `StatusSource` event opens compose in edit mode.
                if let Some(id) = self.selected_own_status_id() {
                    let vis = self
                        .current_target()
                        .map_or(state::Visibility::Public, |t| {
                            api_to_state_vis(t.visibility)
                        });
                    self.pending_edit = Some((id.clone(), vis));
                    let _ = tx.send(Action::LoadSource(id)).await;
                }
            }
            KeyCode::Char('u') => {
                if let Some(acc) = self.current_target().map(|t| t.account.clone()) {
                    self.open_profile(acc, tx).await;
                }
            }
            // `Q` opens the quoted post of the selected status. A
            // dedicated key (vs. reusing `l` / `Enter`) keeps "open
            // outer post" and "open quoted post" unambiguous.
            KeyCode::Char('Q') => {
                if let Some(quoted) = self.selected_quoted_status() {
                    self.open_detail(quoted, tx).await;
                }
            }
            KeyCode::Char('o') if !in_profile => {
                if let Some(url) = self.current_target_url() {
                    match open::that_detached(&url) {
                        Ok(()) => self.push_toast(ToastLevel::Info, "opened in browser".into()),
                        Err(e) => {
                            self.push_toast(
                                ToastLevel::Error,
                                format!("couldn't open browser · {e}"),
                            );
                        }
                    }
                }
            }
            KeyCode::Char('y') => {
                if let Some(url) = self.current_target_url() {
                    match crate::util::clipboard::copy(&url) {
                        Ok(()) => self.push_toast(ToastLevel::Info, "link copied".into()),
                        Err(e) => self.push_toast(ToastLevel::Error, format!("copy failed · {e}")),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Push a profile page for `acc` and kick off its fetches.
    pub(super) async fn open_profile(
        &mut self,
        acc: Account,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) {
        let id = acc.id.clone();
        self.push_mode(Mode::Profile(ProfileScreen::new(acc, false)));
        let _ = tx
            .send(Action::LoadProfile {
                id: id.clone(),
                max_id: None,
            })
            .await;
        let _ = tx.send(Action::LoadRelationship(id)).await;
    }

    /// Push a thread page for `focal` and request its context.
    pub(super) async fn open_detail(
        &mut self,
        focal: Status,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) {
        let detail = DetailState::new(focal);
        let id = detail.focal_id().clone();
        self.push_mode(Mode::StatusDetail(detail));
        let _ = tx.send(Action::OpenStatus(id)).await;
    }

    /// Build a fresh DetailState seeded with the focal post (the inner
    /// status if the selection points at a boost). Returns `None` if
    /// nothing is selected. The `OpenStatus` action is fired by the
    /// caller so the focal id is known here.
    pub(super) fn open_detail_for_selection(&self) -> Option<DetailState> {
        let target = self.selected_target_status()?;
        Some(DetailState::new(target.clone()))
    }

    /// Read-only sibling of `selected_target_status_mut`. Timeline
    /// tabs only — see [`Self::current_target`] for the mode-aware
    /// version.
    pub(super) fn selected_target_status(&self) -> Option<&Status> {
        let kind = self.active;
        let idx = self.screens.get(&kind)?.selected;
        let outer = self.timelines.get(&kind)?.get(idx)?;
        Some(outer.reblog.as_deref().unwrap_or(outer))
    }

    /// The status the cursor points at in whatever mode the user is
    /// in — inner post for boosts. Covers the four timeline tabs
    /// (notifications resolve to their attached status), the thread
    /// page and the profile page.
    pub(super) fn current_target(&self) -> Option<&Status> {
        match &self.mode {
            Mode::Timeline if self.active == TimelineKind::Notifications => {
                let idx = self
                    .notifications_screen
                    .selected_index(&self.notifications)?;
                let s = self.notifications.get(idx)?.status.as_ref()?;
                Some(s.reblog.as_deref().unwrap_or(s))
            }
            Mode::Timeline => self.selected_target_status(),
            Mode::StatusDetail(d) => d.selected_target(),
            Mode::Profile(p) => p.selected_target(),
            Mode::Search(s) => s.selected_target(),
            _ => None,
        }
    }

    /// Public URL of the current target (falls back to the
    /// ActivityPub URI, which is also a browser-openable URL for
    /// Mastodon-family servers).
    pub(super) fn current_target_url(&self) -> Option<String> {
        let t = self.current_target()?;
        let url = t.url.clone().unwrap_or_else(|| t.uri.clone());
        (!url.is_empty()).then_some(url)
    }

    /// Status id to target for a deletion *if* it belongs to the
    /// signed-in user. `None` when there's no selection, no `me`, or
    /// the author doesn't match.
    pub(super) fn selected_own_status_id(&self) -> Option<StatusId> {
        let me_id = self.me.as_ref().map(|a| a.id.clone())?;
        let target = self.current_target()?;
        if target.account.id == me_id {
            Some(target.id.clone())
        } else {
            None
        }
    }

    /// If the currently selected post carries a quote payload with a
    /// resolved `quoted_status`, return an owned clone of that quoted
    /// status. `None` when there's no selection, no quote, or the
    /// quote's state is not `accepted` (no payload).
    pub(super) fn selected_quoted_status(&self) -> Option<Status> {
        self.current_target()?
            .quote
            .as_ref()?
            .quoted_status
            .as_deref()
            .cloned()
    }

    /// Flip the favourite flag on the currently-selected status' inner
    /// post (following Mastodon convention: favouriting a boost
    /// favourites the original). Returns the API action to dispatch;
    /// `None` if there's no selection.
    pub(super) fn toggle_favourite_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        let currently = target.favourited.unwrap_or(false);
        target.favourited = Some(!currently);
        target.favourites_count = if currently {
            target.favourites_count.saturating_sub(1)
        } else {
            target.favourites_count.saturating_add(1)
        };
        let id = target.id.clone();
        Some(if currently {
            Action::Unfavourite(id)
        } else {
            Action::Favourite(id)
        })
    }

    pub(super) fn toggle_reblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        let currently = target.reblogged.unwrap_or(false);
        target.reblogged = Some(!currently);
        target.reblogs_count = if currently {
            target.reblogs_count.saturating_sub(1)
        } else {
            target.reblogs_count.saturating_add(1)
        };
        let id = target.id.clone();
        Some(if currently {
            Action::Unreblog(id)
        } else {
            Action::Reblog(id)
        })
    }

    pub(super) fn force_unreblog_optimistic(&mut self) -> Option<Action> {
        let target = self.selected_target_status_mut()?;
        if !target.reblogged.unwrap_or(false) {
            return None;
        }
        target.reblogged = Some(false);
        target.reblogs_count = target.reblogs_count.saturating_sub(1);
        let id = target.id.clone();
        Some(Action::Unreblog(id))
    }

    /// Mut-borrow the inner status the current selection points at.
    /// For a reblog, that's `outer.reblog`; otherwise just the outer
    /// status itself.
    pub(super) fn selected_target_status_mut(&mut self) -> Option<&mut Status> {
        let kind = self.active;
        let idx = self.screens.get(&kind)?.selected;
        let list = self.timelines.get_mut(&kind)?;
        let outer = list.get_mut(idx)?;
        if outer.reblog.is_some() {
            outer.reblog.as_deref_mut()
        } else {
            Some(outer)
        }
    }

    pub(super) async fn switch_to(
        &mut self,
        kind: TimelineKind,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) {
        // Tabs 1-4 are a *hard reset*: drop any open sub-page (detail /
        // profile / compose) and clear the back-stack. Pressing 1 from
        // deep inside a thread shouldn't leave breadcrumbs.
        self.back_stack.clear();
        self.mode = Mode::Timeline;
        if self.active == kind {
            return;
        }
        self.active = kind;
        if !self.timelines.contains_key(&kind) {
            let _ = tx.send(Action::LoadTimeline(kind)).await;
        }
    }

    /// Enter Mode::Profile with the logged-in user's profile. Tab-5
    /// behaves like tabs 1-4: a *hard reset*. Any open sub-page is
    /// dropped and the back-stack cleared, so `h` from self-profile
    /// returns to Home (not to whatever the user was accidentally on
    /// before pressing 5). That matches the usual tab-bar mental model
    /// — 5 is a top-level destination, not a navigation step.
    /// Persist the new default account, load its token from the
    /// keyring, and dispatch [`Action::SwitchAccount`]. Also wipes
    /// the local UI caches (timelines, notifications, pending
    /// profile state) so the incoming fresh data isn't polluted by
    /// the previous account's posts. Returns a human-readable error
    /// to show as a toast if any step fails; the caller is expected
    /// to keep the switcher open when that happens.
    pub(super) async fn begin_account_switch(
        &mut self,
        acc: &AccountRef,
        tx: &tokio::sync::mpsc::Sender<Action>,
    ) -> Result<(), String> {
        let token =
            config::load_token(&acc.handle).map_err(|e| format!("keyring lookup failed · {e}"))?;

        // Persist the new default so the next `mastoot run` lands on
        // this account. A failed save is non-fatal for *this* session —
        // the switch below still proceeds.
        self.cfg.default_account = Some(acc.handle.clone());
        self.cfg.default_instance = Some(acc.instance.clone());
        if let Err(e) = self.cfg.save(None) {
            tracing::warn!(?e, "failed to persist account switch to config");
        }

        // Wipe every UI-side cache that held the old session's data.
        self.timelines.clear();
        self.notifications.clear();
        self.notifications_screen.reset();
        for screen in self.screens.values_mut() {
            screen.reset();
        }
        self.me = None;
        self.active = TimelineKind::Home;

        let _ = tx
            .send(Action::SwitchAccount {
                instance: acc.instance.clone(),
                handle: acc.handle.clone(),
                token,
            })
            .await;
        Ok(())
    }

    pub(super) async fn open_self_profile(&mut self, tx: &tokio::sync::mpsc::Sender<Action>) {
        // Reuse an already-open self profile (avoid re-fetching every
        // time the user presses `5`).
        if let Mode::Profile(p) = &self.mode
            && p.is_self
        {
            return;
        }
        let me = self.me.clone();
        let Some(me) = me else {
            self.push_toast(ToastLevel::Warn, "credentials still loading…".into());
            return;
        };
        let id = me.id.clone();
        self.back_stack.clear();
        self.active = TimelineKind::Home;
        self.mode = Mode::Profile(ProfileScreen::new(me, true));
        let _ = tx.send(Action::LoadProfile { id, max_id: None }).await;
    }
}
