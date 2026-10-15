//! A `/video` socket (ADR-0017 V6): one watched source, opened with the
//! token `video/watch` handed out. Binary messages out (`uxduck_media::wire`);
//! the only message in is `keyframe`, sent when the client's decoder failed.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use uxduck_media::wire::{self, Kind};

use super::hub::{Hub, Item, Watcher};
use crate::daemon::engine_task::EngineHandle;

pub async fn serve(
    socket: WebSocket,
    token: String,
    watcher: Arc<Watcher>,
    hub: Arc<Hub>,
    engine: EngineHandle,
) {
    let (mut sink, mut stream) = socket.split();
    let writer = {
        let watcher = watcher.clone();
        tokio::spawn(async move {
            let mut sequence: u32 = 0;
            let mut codec: Option<String> = None;
            while let Some((item, discontinuity)) = watcher.next().await {
                let bytes = match &item {
                    Item::Unit(unit) => {
                        let changed = codec.as_deref() != Some(unit.sps.codec.as_str());
                        if changed {
                            codec = Some(unit.sps.codec.clone());
                        }
                        wire::encode(&wire::Message {
                            kind: Kind::AccessUnit,
                            key: unit.key,
                            discontinuity,
                            sequence,
                            timestamp_us: unit.timestamp_us,
                            codec: changed.then_some(unit.sps.codec.as_str()),
                            payload: &unit.data,
                        })
                    }
                    Item::Klv { timestamp_us, data } => wire::encode(&wire::Message {
                        kind: Kind::Klv,
                        key: false,
                        discontinuity: false,
                        sequence,
                        timestamp_us: *timestamp_us,
                        codec: None,
                        payload: data,
                    }),
                };
                sequence = sequence.wrapping_add(1);
                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.send(Message::Close(None)).await;
        })
    };
    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(text) if text.as_str() == "keyframe" => watcher.wait_for_keyframe(),
            Message::Close(_) => break,
            _ => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    hub.close(&token);
    writer.abort();
    engine.post(move |e| e.video_socket_closed(&token));
}
