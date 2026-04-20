//! Mastodon SSE user stream.
//!
//! Opens `GET /api/v1/streaming/user` with `Authorization: Bearer …` and
//! decodes the event names Mastodon emits on that endpoint. Reconnect
//! policy is the caller's responsibility — this module just surfaces a
//! disconnect as `StreamEvent::Disconnect` and returns `None` from the
//! stream.
//!
//! Wire format recap:
//! ```text
//! event: update
//! data: {"id":"…","content":"…", …}
//!
//! event: delete
//! data: 107012345
//!
//! : keepalive (comment; ignored by the event-source parser)
//! ```
//!
//! For SSE, `data:` is the raw JSON string (not the WebSocket
//! double-encoded form). We still json-decode it ourselves into the
//! relevant entity.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::Stream;
use futures_util::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use secrecy::ExposeSecret;
use tracing::{debug, warn};

use crate::api::client::MastodonClient;
use crate::api::error::{ApiError, ApiResult};
use crate::api::models::{Notification, Status, StatusId};

/// A single decoded streaming event.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A new status hit the timeline.
    Update(Box<Status>),
    /// A status was deleted. Payload is the status id string.
    Delete(StatusId),
    /// A new notification arrived.
    Notification(Box<Notification>),
    /// A boosted status got edited.
    StatusUpdate(Box<Status>),
    /// Server told us filters were reloaded.
    FiltersChanged,
    /// Server announcement / reaction / delete (surfaced as raw for now).
    Announcement(serde_json::Value),
    AnnouncementReaction(serde_json::Value),
    AnnouncementDelete(String),
    /// Direct-message conversation updated.
    Conversation(serde_json::Value),
    /// Connection went down. The caller should reconnect.
    Disconnect,
    /// An event name we don't recognize yet — future-compat.
    Unknown {
        event: String,
        data: String,
    },
}

/// Wraps [`reqwest_eventsource::EventSource`] as a typed stream.
pub struct UserStream {
    inner: EventSource,
    done: bool,
}

impl UserStream {
    /// Open a user stream against the client's instance. The client must
    /// carry a token; anonymous streaming is rejected since Mastodon
    /// 4.2.
    pub async fn open(client: &MastodonClient) -> ApiResult<Self> {
        Self::open_endpoint(client, "/api/v1/streaming/user").await
    }

    /// Open the notification-only stream.
    pub async fn open_notification(client: &MastodonClient) -> ApiResult<Self> {
        Self::open_endpoint(client, "/api/v1/streaming/user/notification").await
    }

    async fn open_endpoint(client: &MastodonClient, path: &str) -> ApiResult<Self> {
        let token = client
            .token()
            .ok_or_else(|| ApiError::OAuth("streaming requires a user token".into()))?;
        let url = client.base_url().join(path)?;
        let http = reqwest::Client::builder()
            .user_agent(crate::api::client::USER_AGENT)
            .build()?;
        let request = http
            .get(url)
            .bearer_auth(token.expose_secret())
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let source = EventSource::new(request)
            .map_err(|e| ApiError::Stream(format!("failed to open stream: {e}")))?;
        Ok(Self {
            inner: source,
            done: false,
        })
    }
}

impl Stream for UserStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        loop {
            let next = futures::ready!(self.inner.poll_next_unpin(cx));
            match next {
                None => {
                    self.done = true;
                    return Poll::Ready(Some(StreamEvent::Disconnect));
                }
                Some(Ok(Event::Open)) => {
                    debug!("SSE stream opened");
                }
                Some(Ok(Event::Message(msg))) => {
                    if let Some(parsed) = decode_event(&msg.event, &msg.data) {
                        return Poll::Ready(Some(parsed));
                    }
                }
                Some(Err(e)) => {
                    warn!(?e, "SSE stream error");
                    self.done = true;
                    return Poll::Ready(Some(StreamEvent::Disconnect));
                }
            }
        }
    }
}

fn decode_event(event: &str, data: &str) -> Option<StreamEvent> {
    match event {
        "update" => match serde_json::from_str::<Status>(data) {
            Ok(s) => Some(StreamEvent::Update(Box::new(s))),
            Err(e) => {
                warn!(?e, "failed to decode `update` payload");
                None
            }
        },
        "delete" => Some(StreamEvent::Delete(StatusId::new(data.trim()))),
        "notification" => match serde_json::from_str::<Notification>(data) {
            Ok(n) => Some(StreamEvent::Notification(Box::new(n))),
            Err(e) => {
                warn!(?e, "failed to decode `notification` payload");
                None
            }
        },
        "status.update" => match serde_json::from_str::<Status>(data) {
            Ok(s) => Some(StreamEvent::StatusUpdate(Box::new(s))),
            Err(e) => {
                warn!(?e, "failed to decode `status.update` payload");
                None
            }
        },
        "filters_changed" => Some(StreamEvent::FiltersChanged),
        "conversation" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::Conversation),
        "announcement" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::Announcement),
        "announcement.reaction" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::AnnouncementReaction),
        "announcement.delete" => Some(StreamEvent::AnnouncementDelete(data.trim().to_string())),
        other => Some(StreamEvent::Unknown {
            event: other.to_string(),
            data: data.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_delete_as_status_id() {
        let ev = decode_event("delete", "  107012345  ").unwrap();
        match ev {
            StreamEvent::Delete(id) => assert_eq!(id.as_str(), "107012345"),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn unknown_event_roundtrips() {
        let ev = decode_event("something_new", "{}").unwrap();
        match ev {
            StreamEvent::Unknown { event, .. } => assert_eq!(event, "something_new"),
            _ => panic!("expected Unknown"),
        }
    }
}
