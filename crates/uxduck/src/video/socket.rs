//! A `/video` socket (ADR-0017 V6): one watched source, opened with the
//! token `video/watch` handed out. Binary messages out (`uxduck_media::wire`).
//! Text messages in: `keyframe`, sent when the client's decoder failed, and
//! `sound on` and `sound off`, which start and stop its sound (ADR-0067
//! M11). Sound goes out with its codec string on the first frame, after
//! each `sound on` and when it changes.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use uxduck_media::audio::{AudioConfig, AudioUnit};
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
    let opened = token.clone();
    engine.post(move |e| e.video_socket_opened(&opened));
    let (mut sink, mut stream) = socket.split();
    let writer = {
        let watcher = watcher.clone();
        tokio::spawn(async move {
            let mut sequence: u32 = 0;
            let mut codec: Option<String> = None;
            // The sound's codec as last said, and in which `sound on`.
            let mut sound_codec: Option<(String, u64)> = None;
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
                    Item::Audio(sound) => {
                        let said = (sound_codec_string(sound), watcher.sound_epoch());
                        let changed = sound_codec.as_ref() != Some(&said);
                        let bytes = wire::encode(&wire::Message {
                            kind: Kind::Sound,
                            key: false,
                            discontinuity,
                            sequence,
                            timestamp_us: sound.timestamp_us,
                            codec: changed.then_some(said.0.as_str()),
                            payload: &sound.data,
                        });
                        if changed {
                            sound_codec = Some(said);
                        }
                        bytes
                    }
                    // The hub keeps the camera's MPEG-TS for recordings (S151).
                    Item::Ts(_) => continue,
                };
                sequence = sequence.wrapping_add(1);
                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
                if let Item::Unit(unit) = &item {
                    watcher.sent(unit, std::time::Instant::now());
                }
            }
            let _ = sink.send(Message::Close(None)).await;
        })
    };
    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(text) => match text.as_str() {
                "keyframe" => watcher.wait_for_keyframe(),
                "sound on" => hub.listen(&token, true),
                "sound off" => hub.listen(&token, false),
                _ => {}
            },
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

/// The codec string the app's `AudioDecoder` takes (ADR-0067 M11):
/// `opus/48000/<channels>`, or `mp4a.40.<object type>/<rate>/<channels>/`
/// and the `AudioSpecificConfig` in hex.
fn sound_codec_string(sound: &AudioUnit) -> String {
    match sound.config {
        AudioConfig::Opus(c) => format!("opus/{}/{}", uxduck_media::opus::RATE, c.channels),
        AudioConfig::Aac(c) => {
            let config = c
                .audio_specific_config()
                .iter()
                .fold(String::new(), |mut hex, b| {
                    let _ = write!(hex, "{b:02x}");
                    hex
                });
            // Channel configuration 7 is 7.1: eight channels.
            let channels = if c.channels == 7 { 8 } else { c.channels };
            format!(
                "mp4a.40.{}/{}/{channels}/{config}",
                c.object_type, c.sample_rate
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_media::aac::AacConfig;
    use uxduck_media::opus::OpusConfig;

    #[test]
    fn sound_codec_strings_name_opus_and_aac_with_its_config() {
        let opus = AudioUnit {
            data: vec![0xFC],
            timestamp_us: 0,
            config: AudioConfig::Opus(OpusConfig { channels: 1 }),
            samples: 960,
        };
        assert_eq!(sound_codec_string(&opus), "opus/48000/1");
        let aac = AudioUnit::aac(
            vec![1],
            0,
            AacConfig {
                object_type: 2,
                frequency_index: 3,
                sample_rate: 48_000,
                channels: 2,
            },
        );
        assert_eq!(sound_codec_string(&aac), "mp4a.40.2/48000/2/1190");
    }

    /// The next binary message: its kind, time, codec and payload.
    async fn next(
        client: &mut (
                 impl StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin
             ),
    ) -> (Kind, u64, Option<String>, Vec<u8>) {
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = message {
                let m = wire::decode(&bytes).unwrap();
                return (
                    m.kind,
                    m.timestamp_us,
                    m.codec.map(str::to_owned),
                    m.payload.to_vec(),
                );
            }
        }
    }

    /// ADR-0067 M11: a socket is sent sound only between its `sound on`
    /// and `sound off`, with the codec on the first frame.
    #[tokio::test]
    async fn a_socket_is_sent_sound_only_after_it_says_sound_on() {
        use axum::extract::ws::WebSocketUpgrade;
        use tokio_tungstenite::tungstenite::Message as Sent;
        use uxduck_media::au::AccessUnit;
        use uxduck_media::nal::SpsInfo;

        let hub = Arc::new(Hub::default());
        hub.allow("t", "camera");
        let (_, watcher) = hub.open("t").unwrap();
        let engine = crate::video::whep::tests::test_engine();
        let app = axum::Router::new().route(
            "/video",
            axum::routing::get({
                let hub = hub.clone();
                move |ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(move |socket| serve(socket, "t".into(), watcher, hub, engine))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/video"))
                .await
                .unwrap();
        let picture = |timestamp_us: u64| {
            Item::Unit(Arc::new(AccessUnit {
                arrived: None,
                data: vec![0, 0, 0, 1, 0x65],
                key: true,
                recovery: false,
                timestamp_us,
                composition_us: 0,
                sps: SpsInfo {
                    id: 0,
                    codec: "avc1.42c01e".into(),
                    coded_width: None,
                    coded_height: None,
                    display_width: None,
                    display_height: None,
                },
            }))
        };
        let opus = |timestamp_us: u64| {
            Item::Audio(Arc::new(AudioUnit {
                data: vec![0xFC, 1, 2],
                timestamp_us,
                config: AudioConfig::Opus(OpusConfig { channels: 1 }),
                samples: 960,
            }))
        };
        hub.publish("camera", &picture(0));
        hub.publish("camera", &opus(10));
        hub.publish("camera", &picture(33));
        assert_eq!(next(&mut client).await.0, Kind::AccessUnit);
        assert_eq!(next(&mut client).await.1, 33, "no sound before sound on");
        let mut wanted = hub.sound_wanted("camera");
        client.send(Sent::text("sound on")).await.unwrap();
        wanted.wait_for(|on| *on).await.unwrap();
        hub.publish("camera", &opus(40));
        hub.publish("camera", &opus(60));
        assert_eq!(
            next(&mut client).await,
            (
                Kind::Sound,
                40,
                Some("opus/48000/1".to_owned()),
                vec![0xFC, 1, 2]
            )
        );
        assert_eq!(
            next(&mut client).await,
            (Kind::Sound, 60, None, vec![0xFC, 1, 2])
        );
        client.send(Sent::text("sound off")).await.unwrap();
        wanted.wait_for(|on| !*on).await.unwrap();
        hub.publish("camera", &opus(80));
        hub.publish("camera", &picture(100));
        assert_eq!(next(&mut client).await.1, 100, "no sound after sound off");
        client.send(Sent::text("sound on")).await.unwrap();
        wanted.wait_for(|on| *on).await.unwrap();
        hub.publish("camera", &opus(120));
        assert_eq!(
            next(&mut client).await.2.as_deref(),
            Some("opus/48000/1"),
            "the codec again after sound on"
        );
    }
}
