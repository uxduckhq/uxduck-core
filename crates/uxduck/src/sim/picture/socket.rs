//! A window's `/sim-camera` socket (S408), and the review that says who
//! draws: four times a second while any simulation carries a camera, and
//! not at all while none does.

use std::collections::HashSet;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use uxduck_core::domain::video::VideoState;

use super::wire::{self, FromWindow};
use super::{pictures, source};
use crate::daemon::engine_task::EngineHandle;

const REVIEW_EVERY: Duration = Duration::from_millis(250);
/// A camera's stream is looked for among the video sources this often,
/// in reviews.
const LOOK_EVERY: u32 = 4;

/// Serves one window until its socket closes.
pub async fn serve(socket: WebSocket) {
    let pictures = pictures();
    let (window, mut to) = pictures.join();
    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(message) = to.recv().await {
            if sink.send(Message::Binary(message.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });
    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Binary(bytes) => match wire::read(&bytes) {
                Some(FromWindow::Able(able)) => pictures.able(window, able),
                Some(FromWindow::Picture(picture)) => pictures.picture(window, &picture),
                None => {}
            },
            Message::Close(_) => break,
            _ => {}
        }
        if writer.is_finished() {
            break;
        }
    }
    pictures.left(window);
    writer.abort();
}

/// Starts the review. A camera is wanted while its video source is watched
/// or recorded, which the video hub knows. One nobody can draw has its
/// source say why. One whose source the engine does not have yet makes
/// itself known to the listener.
pub fn start(engine: EngineHandle) {
    tokio::spawn(async move {
        let pictures = pictures();
        let mut every = tokio::time::interval(REVIEW_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reviews = 0u32;
        loop {
            let ports = pictures.ports();
            if ports.is_empty() {
                pictures.carried().await;
                continue;
            }
            every.tick().await;
            crate::wakes::count("simulated camera pictures (4 Hz)");
            let hub = crate::video::hub();
            let silent =
                pictures.review(|port| hub.as_ref().is_some_and(|hub| hub.needed(&source(port))));
            for (port, why) in silent {
                let id = source(port);
                engine.post(move |e| {
                    // Once the listener has heard the stream end, and not
                    // before. For three seconds after the last picture it
                    // still says the camera sends, and a reason given then
                    // would be taken back and given again, each time ending
                    // one recording and starting another of nothing.
                    let stopped = e
                        .state()
                        .video_sources
                        .get(&id)
                        .is_some_and(|source| source.state == VideoState::Stopped);
                    if stopped {
                        e.video_stopped_because(&id, Some(why.says().to_owned()));
                    }
                });
            }
            reviews = reviews.wrapping_add(1);
            if !reviews.is_multiple_of(LOOK_EVERY) {
                continue;
            }
            let Ok(heard) = engine
                .run(|e| {
                    e.state()
                        .video_sources
                        .values()
                        .filter(|s| s.url.is_none() && s.capture.is_none() && s.file.is_none())
                        .map(|s| s.port)
                        .collect::<HashSet<u16>>()
                })
                .await
            else {
                return;
            };
            for port in ports.into_iter().filter(|port| !heard.contains(port)) {
                pictures.hello(port);
            }
        }
    });
}
