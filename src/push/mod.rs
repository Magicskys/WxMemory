use crate::monitor::whitelist::{ChatKind, Entry};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::get,
};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::broadcast;

const BUFFER_SIZE: usize = 128;
const HISTORY_SIZE: usize = 1024;

/// A message source can publish only after identifying the originating conversation.
#[allow(dead_code)]
#[derive(Clone, Serialize)]
pub struct Message {
    pub chat_kind: ChatKind,
    pub chat_id: String,
    pub sender_id: String,
    pub content: String,
    pub timestamp: i64,
    #[serde(skip)]
    pub identity: Option<String>,
}

#[derive(Clone)]
struct StoredMessage {
    timestamp: i64,
    is_whitelisted: bool,
    json: String,
    identity: Option<String>,
}

struct BroadcastState {
    sender: broadcast::Sender<StoredMessage>,
    history: VecDeque<StoredMessage>,
    // Greatest timestamp evicted so a replay never silently claims completeness.
    max_evicted_timestamp: Option<i64>,
    started_at: i64,
}

struct HistorySource {
    root: std::path::PathBuf,
    key: String,
}

#[derive(Clone)]
pub struct PushService {
    whitelist: Arc<[Entry]>,
    state: Arc<Mutex<BroadcastState>>,
    history_source: Option<Arc<HistorySource>>,
}

impl PushService {
    pub fn new(whitelist: Vec<Entry>) -> Self {
        let (sender, _) = broadcast::channel(BUFFER_SIZE);
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Self {
            whitelist: whitelist.into(),
            history_source: None,
            state: Arc::new(Mutex::new(BroadcastState {
                sender,
                history: VecDeque::new(),
                max_evicted_timestamp: None,
                started_at,
            })),
        }
    }

    pub fn with_history(mut self, root: std::path::PathBuf, key: String) -> Self {
        self.history_source = Some(Arc::new(HistorySource { root, key }));
        self
    }

    /// Broadcasts to subscribers and reports whether the message matches the watchlist.
    pub fn publish(&self, message: &Message) -> Result<bool, serde_json::Error> {
        let is_whitelisted = self
            .whitelist
            .iter()
            .any(|entry| entry.kind == message.chat_kind && entry.id == message.chat_id);
        let item = StoredMessage {
            timestamp: message.timestamp,
            identity: message.identity.clone(),
            is_whitelisted,
            json: serde_json::to_string(message)?,
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.history.len() == HISTORY_SIZE
            && let Some(evicted) = state.history.pop_front()
        {
            state.max_evicted_timestamp = Some(
                state
                    .max_evicted_timestamp
                    .map_or(evicted.timestamp, |old| old.max(evicted.timestamp)),
            );
        }
        state.history.push_back(item.clone());
        let _ = state.sender.send(item);
        Ok(is_whitelisted)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    since: Option<i64>,
    #[serde(default = "default_watchlist")]
    watchlist: bool,
}

fn default_watchlist() -> bool {
    true
}

pub fn router(service: PushService) -> Router {
    Router::new()
        .route("/api/events", get(events))
        .with_state(service)
}

async fn events(
    State(service): State<PushService>,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, &'static str)> {
    let (mut history, mut receiver) = {
        let state = service
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if service.history_source.is_none()
            && let Some(since) = query.since
            && (since < state.started_at
                || state
                    .max_evicted_timestamp
                    .is_some_and(|timestamp| since < timestamp))
        {
            return Err((StatusCode::GONE, "请求时间超出服务可补发的缓存范围"));
        }
        let receiver = state.sender.subscribe();
        let history: Vec<_> = query
            .since
            .map(|since| {
                state
                    .history
                    .iter()
                    .filter(|item| {
                        item.timestamp > since && (!query.watchlist || item.is_whitelisted)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (history, receiver)
    };
    if let (Some(since), Some(source)) = (query.since, service.history_source.clone()) {
        let whitelist = service.whitelist.clone();
        let watchlist = query.watchlist;
        let messages = tokio::task::spawn_blocking(move || {
            crate::monitor::reader::history(
                &source.root,
                &source.key,
                since,
                if watchlist { Some(&whitelist) } else { None },
            )
        })
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "历史消息查询异常"))?
        .map_err(|error| {
            eprintln!("历史消息查询失败：{error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "历史消息查询失败，请检查服务日志",
            )
        })?;
        history = messages
            .into_iter()
            .map(|message| {
                let json = serde_json::to_string(&message)
                    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "历史消息编码失败"))?;
                Ok(StoredMessage {
                    timestamp: message.timestamp,
                    is_whitelisted: true,
                    identity: message.identity,
                    json,
                })
            })
            .collect::<Result<Vec<_>, (StatusCode, &'static str)>>()?;
    }
    let mut replay_ids: std::collections::HashSet<_> = history
        .iter()
        .filter_map(|item| item.identity.clone())
        .collect();
    let mut pending = Vec::new();
    if query.since.is_some() && service.history_source.is_some() {
        loop {
            match receiver.try_recv() {
                Ok(item) => {
                    if (!query.watchlist || item.is_whitelisted)
                        && query.since.is_none_or(|since| item.timestamp > since)
                        && !item
                            .identity
                            .as_ref()
                            .is_some_and(|id| replay_ids.remove(id))
                    {
                        pending.push(item);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        "历史查询期间实时消息缓冲区溢出，请重试",
                    ));
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err((StatusCode::SERVICE_UNAVAILABLE, "消息服务已关闭"));
                }
            }
        }
    }
    let replay = stream::iter(
        history
            .into_iter()
            .chain(pending)
            .map(|item| Ok(Event::default().event("message").data(item.json))),
    );
    let live = stream::unfold(
        (receiver, query.since, query.watchlist, replay_ids),
        |(mut receiver, since, watchlist, mut replay_ids)| async move {
            loop {
                match receiver.recv().await {
                    Ok(item)
                        if (!watchlist || item.is_whitelisted)
                            && since.is_none_or(|since| item.timestamp > since) =>
                    {
                        if item
                            .identity
                            .as_ref()
                            .is_some_and(|id| replay_ids.remove(id))
                        {
                            continue;
                        }
                        return Some((
                            Ok(Event::default().event("message").data(item.json)),
                            (receiver, since, watchlist, replay_ids),
                        ));
                    }
                    Ok(_) => continue,
                    Err(_) => return None,
                }
            }
        },
    );
    Ok(Sse::new(replay.chain(live)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;

    #[tokio::test]
    async fn streams_only_whitelisted_events_to_client() {
        let service = PushService::new(vec![Entry::new(ChatKind::User, "wxid_allowed").unwrap()]);
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let mut body = response.into_body();
        let mut message = Message {
            chat_kind: ChatKind::Group,
            chat_id: "wxid_allowed".into(),
            sender_id: "sender".into(),
            content: "hello\nworld".into(),
            timestamp: 123,
            identity: None,
        };
        assert!(!service.publish(&message).unwrap());
        message.chat_kind = ChatKind::User;
        message.chat_id = "not_allowed".into();
        assert!(!service.publish(&message).unwrap());
        message.chat_id = "wxid_allowed".into();
        assert!(service.publish(&message).unwrap());
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let bytes = frame.into_data().unwrap();
        let event = std::str::from_utf8(&bytes).unwrap();
        assert!(event.contains("event: message"));
        assert!(event.contains("wxid_allowed") && event.contains(r"hello\nworld"));
        assert!(!event.contains("not_allowed"));
    }
    #[tokio::test]
    async fn replays_then_streams_with_strict_timestamp_and_whitelist() {
        let service = PushService::new(vec![Entry::new(ChatKind::User, "allowed").unwrap()]);
        service.state.lock().unwrap().started_at = 100;
        let mut message = Message {
            chat_kind: ChatKind::User,
            chat_id: "allowed".into(),
            sender_id: "s".into(),
            content: "old".into(),
            timestamp: 100,
            identity: None,
        };
        service.publish(&message).unwrap();
        message.timestamp = 101;
        message.content = "replay".into();
        service.publish(&message).unwrap();
        message.chat_id = "blocked".into();
        message.content = "secret".into();
        assert!(!service.publish(&message).unwrap());
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        message.chat_id = "allowed".into();
        message.timestamp = 102;
        message.content = "live".into();
        service.publish(&message).unwrap();
        for expected in ["replay", "live"] {
            let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            let text = std::str::from_utf8(&frame).unwrap();
            assert!(text.contains(expected));
            assert!(!text.contains("secret"));
        }
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=99")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE);
        let response = router(service)
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=invalid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn refuses_incomplete_replay_after_eviction() {
        let service = PushService::new(vec![Entry::new(ChatKind::User, "allowed").unwrap()]);
        service.state.lock().unwrap().started_at = 0;
        let message = Message {
            chat_kind: ChatKind::User,
            chat_id: "allowed".into(),
            sender_id: "s".into(),
            content: String::new(),
            timestamp: 2,
            identity: None,
        };
        for _ in 0..=HISTORY_SIZE {
            service.publish(&message).unwrap();
        }
        let app = router(service);
        let old = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::GONE);
        let boundary = app
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(boundary.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn watchlist_false_includes_other_sessions_in_replay_and_live() {
        let service = PushService::new(vec![Entry::new(ChatKind::User, "allowed").unwrap()]);
        service.state.lock().unwrap().started_at = 100;
        let mut message = Message {
            chat_kind: ChatKind::User,
            chat_id: "other".into(),
            sender_id: "s".into(),
            content: "replay-other".into(),
            timestamp: 101,
            identity: None,
        };
        assert!(!service.publish(&message).unwrap());
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=100&watchlist=false")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        message.content = "live-other".into();
        message.timestamp = 102;
        service.publish(&message).unwrap();
        for expected in ["replay-other", "live-other"] {
            let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            assert!(std::str::from_utf8(&frame).unwrap().contains(expected));
        }
        let response = router(service)
            .oneshot(
                Request::builder()
                    .uri("/api/events?watchlist=invalid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    #[tokio::test]
    async fn sends_no_initial_keep_alive_and_old_route_is_missing() {
        let service = PushService::new(Vec::new());
        let old = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::NOT_FOUND);
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), body.frame())
                .await
                .is_err()
        );
    }
}
