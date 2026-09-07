//! axum_impl::sse — see parent module docs.

use super::*;

pub(crate) fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if e == epoch && c == channel.as_str() {
        seq
    } else {
        0
    }
}

pub(crate) fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|v| v.parse::<u64>().ok());
    if e == epoch && kind == "timeline" && parts.next().is_none() {
        seq.unwrap_or(0)
    } else {
        0
    }
}

fn envelope_to_event(
    epoch: &str,
    channel: RingingChannel,
    env: &qaqh_ringing::RingingEventEnvelope,
) -> Event {
    let event_type = serde_json::to_value(&env.event)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "message".into());
    // data is full envelope JSON (renderer expects seed/stream_seq/event_id)
    let data = serde_json::to_string(env).unwrap_or_else(|_| "{}".into());
    Event::default()
        .id(format!("{}:{}:{}", epoch, channel.as_str(), env.stream_seq))
        .event(event_type)
        .data(data)
}

fn reset_to_event(reset: &RingingResetRequired) -> Event {
    let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
    Event::default().event("ringing.reset_required").data(data)
}

fn timeline_entry_to_event(epoch: &str, seed: &str, entry: &qaqh_domain::TimelineEntry) -> Event {
    let data = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 1,
        "server_epoch": epoch,
        "seed": seed,
        "entry": entry,
    });
    Event::default()
        .id(format!("{}:timeline:{}", epoch, entry.timeline_seq))
        .event("timeline.entry")
        .json_data(data)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

fn filter_replay_for_session(
    mut replay: qaqh_runtime::ringing::hub::ChannelReplay,
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> qaqh_runtime::ringing::hub::ChannelReplay {
    let mut g = leases.lock().unwrap_or_else(|e| e.into_inner());
    replay.events.retain(|e| g.owns_seed(session_id, &e.seed));
    replay.resets.retain(|r| g.owns_seed(session_id, &r.seed));
    replay
}

// ---- SSE handlers (P2) ----
pub(crate) async fn handle_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_str): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if !state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active_session(&session_id)
    {
        return lease_required_json();
    }
    let Some(channel) = parse_channel(&channel_str) else {
        return (StatusCode::NOT_FOUND, "unknown channel").into_response();
    };
    // Last-Event-ID from header or ?last_event_id= query (ringing_http compat)
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after_seq = parse_sse_cursor(last_event_id, &state.epoch, channel);

    // Subscribe before replay to avoid gap
    let rx = state.hub.subscribe(channel);
    let replay = filter_replay_for_session(
        state
            .hub
            .replay_channel_since(channel, after_seq, after_seq == 0),
        &session_id,
        &state.leases,
    );
    let replayed_ids: HashSet<String> = replay.events.iter().map(|e| e.event_id.clone()).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let session_id_clone = session_id.clone();

    // Use mpsc channel to bridge broadcast to Sse stream (keeps axum 0.8 Send + 'static)
    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        // Replay
        for env in replay.events {
            let ev = envelope_to_event(&epoch, channel, &env);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        for reset in replay.resets {
            let ev = reset_to_event(&reset);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .owns_seed(&session_id_clone, &envelope.seed)
                    {
                        continue;
                    }
                    if envelope.stream_seq <= after_seq || replayed_ids.contains(&envelope.event_id)
                    {
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&session_id_clone)
                    {
                        break;
                    }
                    let ev = envelope_to_event(&epoch, channel, &envelope);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx_stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

pub(crate) async fn handle_timeline_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if seed.is_empty()
        || !state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required"}"#.to_vec(),
        )
            .into_response();
    }
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after = parse_timeline_cursor(last_event_id, &state.epoch);
    let rx = state.hub.subscribe_timeline();
    let replay = state.hub.timeline_replay_since(&seed, after);
    let replayed: HashSet<u64> = replay.iter().map(|e| e.timeline_seq).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let seed_clone = seed.clone();
    let session_id_clone = session_id.clone();

    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        for entry in replay {
            let ev = timeline_entry_to_event(&epoch, &seed_clone, &entry);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    if live.seed != seed_clone
                        || live.entry.timeline_seq <= after
                        || replayed.contains(&live.entry.timeline_seq)
                    {
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&session_id_clone)
                    {
                        break;
                    }
                    let ev = timeline_entry_to_event(&epoch, &seed_clone, &live.entry);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx_stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ---- debug static (P3) ----
