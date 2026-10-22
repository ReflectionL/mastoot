//! Folding state-task [`Event`]s into the UI's own state.

use super::{
    Action, ApiHealth, App, ComposeLimits, ComposeState, Event, Mode, Status, TimelineKind,
    ToastLevel, apply_revert, missing_parents,
};

impl App {
    pub(super) fn handle_event(&mut self, event: Event) {
        if self.splash {
            let dismiss = match &event {
                Event::TimelineUpdated {
                    kind: TimelineKind::Home,
                    ..
                } => true,
                Event::ApiHealthChanged(h) => *h != ApiHealth::Healthy,
                _ => false,
            };
            if dismiss {
                self.splash = false;
            }
        }
        match event {
            Event::TimelineUpdated {
                kind,
                statuses,
                appended,
            } => {
                let slot = self.timelines.entry(kind).or_default();
                if appended {
                    let known: std::collections::HashSet<_> =
                        slot.iter().map(|s| s.id.clone()).collect();
                    for s in statuses {
                        if !known.contains(&s.id) {
                            slot.push(s);
                        }
                    }
                } else {
                    *slot = statuses;
                }
                let len = slot.len();
                if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_items_changed(len, appended);
                }
                if kind == TimelineKind::Home {
                    let wanted = missing_parents(
                        self.timelines.get(&kind).map_or(&[][..], Vec::as_slice),
                        &self.parents,
                        &self.parents_requested,
                    );
                    for pid in wanted {
                        self.parents_requested.insert(pid.clone());
                        self.pending_actions.push(Action::LoadStatus(pid));
                    }
                }
            }
            Event::StatusSource(src) => {
                if let Some((id, vis)) = self.pending_edit.take()
                    && id == src.id
                    && !matches!(self.mode, Mode::Compose(_) | Mode::ComposeConfirmDiscard(_))
                {
                    self.push_mode(Mode::Compose(ComposeState::edit(src, vis, self.limits)));
                }
            }
            Event::StatusLoaded(status) => {
                self.parents_requested.remove(&status.id);
                self.parents.insert(status.id.clone(), status);
            }
            Event::StatusLoadFailed(id) => {
                // Stays in `parents_requested` on purpose: don't retry.
                tracing::debug!(%id, "parent status unavailable");
            }
            Event::StatusUpdated(status) => {
                // The update may apply to a status that lives inside a
                // boost (e.g. favouriting someone else's boosted post:
                // the action targets the inner id, so match both outer
                // and nested reblog ids).
                for list in self.timelines.values_mut() {
                    for slot in list.iter_mut() {
                        if slot.id == status.id {
                            *slot = status.clone();
                        } else if let Some(inner) = slot.reblog.as_deref_mut()
                            && inner.id == status.id
                        {
                            *inner = status.clone();
                        }
                    }
                }
                if let Mode::StatusDetail(state) = &mut self.mode {
                    state.on_status_updated(&status);
                }
                if let Mode::Profile(p) = &mut self.mode {
                    p.on_status_updated(&status);
                }
                if let Mode::Search(s) = &mut self.mode {
                    s.on_status_updated(&status);
                }
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.on_status_updated(&status),
                        Mode::Profile(p) => p.on_status_updated(&status),
                        Mode::Search(s) => s.on_status_updated(&status),
                        _ => {}
                    }
                }
            }
            Event::StatusContext {
                focal_id,
                ancestors,
                descendants,
            } => {
                if let Mode::StatusDetail(state) = &mut self.mode
                    && state.focal_id() == &focal_id
                {
                    state.on_context_loaded(ancestors, descendants);
                }
            }
            Event::InstanceLoaded {
                max_characters,
                max_media_attachments,
            } => {
                // A switch to an instance that omits a field falls back
                // to the default rather than keeping the old server's.
                let d = ComposeLimits::default();
                self.limits = ComposeLimits {
                    max_chars: max_characters.map_or(d.max_chars, |n| n as usize),
                    max_attachments: max_media_attachments
                        .map_or(d.max_attachments, |n| n as usize),
                };
            }
            Event::CredentialsLoaded(account) => {
                self.me = Some(account);
            }
            Event::ProfileLoaded {
                account,
                statuses,
                appended,
            } => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == account.id
                {
                    p.on_loaded(account, statuses, appended);
                    let wanted =
                        missing_parents(&p.statuses, &self.parents, &self.parents_requested);
                    for pid in wanted {
                        self.parents_requested.insert(pid.clone());
                        self.pending_actions.push(Action::LoadStatus(pid));
                    }
                }
            }
            Event::RelationshipLoaded(rel) => {
                if let Mode::Profile(p) = &mut self.mode {
                    p.on_relationship_loaded(rel.clone());
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev {
                        p.on_relationship_loaded(rel.clone());
                    }
                }
            }
            Event::AccountListLoaded {
                for_id,
                kind,
                accounts,
                appended,
            } => {
                if let Mode::AccountList(state) = &mut self.mode
                    && state.for_id == for_id
                    && state.kind == kind
                {
                    state.on_loaded(accounts, appended);
                }
            }
            Event::RelationshipActionFailed {
                id,
                attempted_follow,
            } => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == id
                {
                    p.revert_follow_action(attempted_follow);
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev
                        && p.account_id == id
                    {
                        p.revert_follow_action(attempted_follow);
                    }
                }
            }
            Event::StatusActionFailed { id, action } => {
                // Walk every cached status that *might* hold this id —
                // outer or inner reblog — across all timelines and the
                // open detail page, and reverse the optimistic flip.
                for list in self.timelines.values_mut() {
                    for slot in list.iter_mut() {
                        if slot.id == id {
                            apply_revert(slot, action);
                        } else if let Some(inner) = slot.reblog.as_deref_mut()
                            && inner.id == id
                        {
                            apply_revert(inner, action);
                        }
                    }
                }
                if let Mode::StatusDetail(state) = &mut self.mode {
                    state.revert_action(&id, action);
                }
                if let Mode::Profile(p) = &mut self.mode {
                    p.revert_action(&id, action);
                }
                if let Mode::Search(s) = &mut self.mode {
                    s.revert_action(&id, action);
                }
                // Sub-pages stashed in the back-stack also need patching
                // so the user doesn't see stale optimistic state when
                // they navigate back.
                for prev in &mut self.back_stack {
                    match prev {
                        Mode::StatusDetail(d) => d.revert_action(&id, action),
                        Mode::Profile(p) => p.revert_action(&id, action),
                        Mode::Search(s) => s.revert_action(&id, action),
                        _ => {}
                    }
                }
            }
            Event::StatusDeleted(id) => {
                let gone = |s: &Status| s.id == id || s.reblog.as_ref().is_some_and(|r| r.id == id);
                for (kind, list) in &mut self.timelines {
                    list.retain(|s| !gone(s));
                    if let Some(screen) = self.screens.get_mut(kind) {
                        screen.on_len_changed(list.len());
                    }
                }
                self.notifications
                    .retain(|n| n.status.as_ref().is_none_or(|s| !gone(s)));
                self.notifications_screen
                    .on_len_changed(self.notifications.len());
                // Thread / profile pages — the live one and any
                // stashed on the back-stack. A thread whose *focal*
                // was deleted has nothing left to anchor on: drop it
                // from the stack, and if it's the live page, leave.
                let mut leave = false;
                match &mut self.mode {
                    Mode::StatusDetail(d) => leave = d.on_status_deleted(&id),
                    Mode::Profile(p) => p.on_status_deleted(&id),
                    Mode::Search(s) => s.on_status_deleted(&id),
                    _ => {}
                }
                self.back_stack.retain_mut(|prev| match prev {
                    Mode::StatusDetail(d) => !d.on_status_deleted(&id),
                    Mode::Profile(p) => {
                        p.on_status_deleted(&id);
                        true
                    }
                    Mode::Search(s) => {
                        s.on_status_deleted(&id);
                        true
                    }
                    _ => true,
                });
                if leave {
                    self.pop_mode();
                }
            }
            Event::LoadMoreFailed(kind) => {
                if kind == TimelineKind::Notifications {
                    self.notifications_screen.on_load_more_failed();
                } else if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_load_more_failed();
                }
            }
            Event::ProfileLoadFailed(id) => {
                if let Mode::Profile(p) = &mut self.mode
                    && p.account_id == id
                {
                    p.on_load_failed();
                }
                for prev in &mut self.back_stack {
                    if let Mode::Profile(p) = prev
                        && p.account_id == id
                    {
                        p.on_load_failed();
                    }
                }
            }
            Event::AccountListLoadFailed { for_id, kind } => {
                if let Mode::AccountList(l) = &mut self.mode
                    && l.for_id == for_id
                    && l.kind == kind
                {
                    l.on_load_failed();
                }
            }
            Event::TimelineStatusAdded { kind, status } => {
                let slot = self.timelines.entry(kind).or_default();
                // Dedup — reconnects replay recent events, and the
                // hot-start fetch may have already pulled this id.
                if slot.iter().any(|s| s.id == status.id) {
                    return;
                }
                slot.insert(0, status.clone());
                let new_len = slot.len();
                if let Some(screen) = self.screens.get_mut(&kind) {
                    screen.on_prepended(1, new_len);
                }
                if kind == TimelineKind::Home {
                    self.request_parents(std::slice::from_ref(&status));
                }
            }
            Event::NotificationsUpdated { items, appended } => {
                if appended {
                    let known: std::collections::HashSet<_> =
                        self.notifications.iter().map(|n| n.id.clone()).collect();
                    for n in items {
                        if !known.contains(&n.id) {
                            self.notifications.push(n);
                        }
                    }
                } else {
                    self.notifications = items;
                }
                self.notifications_screen
                    .on_items_changed(self.notifications.len(), appended);
            }
            Event::MediaUploaded { local_id, result } => {
                // The compose screen may be showing, or sitting behind
                // the discard-confirm; anywhere else the draft is gone
                // and the answer has nobody to tell.
                let (Mode::Compose(c) | Mode::ComposeConfirmDiscard(c)) = &mut self.mode else {
                    return;
                };
                let failure = result.as_ref().err().cloned();
                if c.on_upload_result(local_id, result)
                    && let Some(why) = failure
                {
                    self.push_toast(ToastLevel::Error, format!("upload failed · {why}"));
                }
            }
            Event::NotificationReceived(n) => {
                if self
                    .notifications
                    .iter()
                    .any(|existing| existing.id == n.id)
                {
                    return;
                }
                self.notifications.insert(0, n);
                self.notifications_screen.on_prepended(&self.notifications);
            }
            Event::SearchResults { query, results } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_results(results);
                }
            }
            Event::SearchStatuses { query, statuses } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_statuses(statuses);
                }
            }
            Event::SearchFailed { query } => {
                if let Mode::Search(s) = &mut self.mode
                    && s.query == query
                {
                    s.on_failed();
                }
            }
            Event::Toast { level, message } => {
                self.push_toast(level, message);
            }
            Event::StreamState(s) => {
                self.stream = s;
            }
            Event::ApiHealthChanged(h) => {
                self.api_health = h;
            }
            Event::AccountSwitched { handle } => {
                // UI-side caches were already wiped in
                // `begin_account_switch`. This event is confirmation
                // from the state task — a sanity refresh in case the
                // switch also raced past an in-flight `StatusUpdated`
                // or similar from the outgoing session.
                self.timelines.clear();
                self.notifications.clear();
                self.parents.clear();
                self.parents_requested.clear();
                self.back_stack.clear();
                self.active = TimelineKind::Home;
                for screen in self.screens.values_mut() {
                    screen.reset();
                }
                self.notifications_screen.reset();
                tracing::debug!(%handle, "account switched");
            }
        }
    }
}
