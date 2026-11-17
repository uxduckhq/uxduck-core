//! The assistant's look through a camera (FB-244): the picture now, or a
//! short clip of frames, as JPEGs it is shown. The daemon never decodes
//! (ADR-0017 V1): the capture helper does, from the camera's stream on the
//! hub, and only JPEGs come back. While a look waits, the camera counts as
//! watched, so a camera nobody had open connects for it.

use serde_json::{Value, json};
use uxduck_contracts::dto::video::{
    LOOK_LONG_EDGE, LOOK_REPLY_BYTES, LOOK_SECONDS_DEFAULT, VideoFrame, VideoFrameParams,
    VideoFrameResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::settings::AgentCameras;
use uxduck_core::domain::video::{VideoSource, VideoState};

use crate::{ConnectionId, Engine};

mod json_rpc {
    use serde::Serialize;
    use serde_json::Value;
    use uxduck_contracts::dto::video::VideoFrameResult;

    #[derive(Serialize)]
    pub(super) struct Reply<'a> {
        pub(super) jsonrpc: &'static str,
        pub(super) id: &'a Value,
        pub(super) result: &'a VideoFrameResult,
    }
}

/// What the daemon's video host is asked to do for one look.
#[derive(Clone, Debug, PartialEq)]
pub struct LookOrder {
    pub id: String,
    pub source_id: String,
    pub camera: String,
    pub frames: u32,
    /// The clip's length; 0 for one frame.
    pub seconds: f64,
    pub max_bytes: Option<u64>,
}

/// One frame the helper made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookedFrame {
    pub offset_us: u64,
    pub at_us: u64,
    pub width: u32,
    pub height: u32,
    /// The JPEG's size, before base64.
    pub jpeg_bytes: u64,
    /// The JPEG, base64: the video host encodes it, so the engine's thread
    /// only moves it into the reply.
    pub jpeg_base64: String,
}

/// What the helper made of a look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Looked {
    pub camera_width: Option<u32>,
    pub camera_height: Option<u32>,
    pub frames: Vec<LookedFrame>,
}

/// A look waiting for the helper.
#[derive(Debug)]
pub(crate) struct Waiting {
    connection: ConnectionId,
    request_id: Value,
    pub(crate) source_id: String,
    max_bytes: Option<u64>,
    /// What was changed from what was asked, for the answer's note.
    kept: Option<String>,
}

/// Where a person changes whether looks are allowed, as the app names it.
pub const CAMERAS_SETTING: &str = "Settings › Privacy › Assistant sees cameras";

/// What a look is asked for, checked: how many frames over how long.
pub(crate) struct Asked {
    pub(crate) frames: u32,
    pub(crate) seconds: f64,
}

/// The frames and seconds a request asks for, checked.
///
/// # Errors
/// What is wrong with them, in words.
pub(crate) fn asked(params: &VideoFrameParams) -> Result<Asked, ApiError> {
    let frames = params.frames.unwrap_or(1);
    if frames == 0 {
        return Err(invalid(
            "frames is 1 for the picture now, or 2 or more for a clip. Each frame is sent to the \
             AI model as an image.",
        ));
    }
    let seconds = match (frames, params.seconds) {
        (1, None) => 0.0,
        (1, Some(_)) => {
            return Err(invalid(
                "seconds is a clip's length. Ask for 2 frames or more with it.",
            ));
        }
        (_, None) => LOOK_SECONDS_DEFAULT,
        (_, Some(s)) if s.is_finite() && s > 0.0 => s,
        (_, Some(_)) => {
            return Err(invalid("seconds is a clip's length, more than 0."));
        }
    };
    Ok(Asked { frames, seconds })
}

/// A count and a noun, such as "1 picture" or "12 pictures".
fn count(n: u32, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// Seconds as a person reads them: "3 seconds", "1.5 seconds", "2 minutes".
pub(crate) fn duration_words(seconds: f64) -> String {
    let rounded = |value: f64| {
        let text = format!("{value:.1}");
        text.strip_suffix(".0").map_or(text.clone(), str::to_owned)
    };
    if seconds >= 120.0 {
        format!("{} minutes", rounded(seconds / 60.0))
    } else if (seconds - 1.0).abs() < f64::EPSILON {
        "1 second".to_owned()
    } else {
        format!("{} seconds", rounded(seconds))
    }
}

/// What a look asks for, in words for the person allowing it: "the
/// picture now" or "12 pictures over 30 seconds".
pub(crate) fn asked_words(asked: &Asked) -> String {
    if asked.frames == 1 {
        "the picture now".to_owned()
    } else {
        format!(
            "{} over {}",
            count(asked.frames, "picture"),
            duration_words(asked.seconds)
        )
    }
}

/// A clip's frames, no more than the camera sends pictures in the clip's
/// time: more would repeat pictures. With a note of what was kept.
fn frames_the_camera_sends(
    asked: &Asked,
    frame_rate: Option<f64>,
    camera: &str,
) -> (u32, Option<String>) {
    let Some(rate) = frame_rate.filter(|r| r.is_finite() && *r > 0.0) else {
        return (asked.frames, None);
    };
    let sent = (asked.seconds * rate).floor() + 1.0;
    if asked.frames == 1 || f64::from(asked.frames) <= sent {
        return (asked.frames, None);
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "fewer than the frames asked for, which is a u32"
    )]
    let kept = sent as u32;
    (
        kept,
        Some(format!(
            "Made {} of the {} asked for: {camera} sends {} pictures a second, so more over {} \
             would repeat pictures.",
            count(kept, "frame"),
            asked.frames,
            (rate * 10.0).round() / 10.0,
            duration_words(asked.seconds)
        )),
    )
}

/// JPEG bytes a frame of `pixels` takes at the helper's quality, measured
/// on camera footage (1600 × 904 scaled to 1024, 640 and 320 px wide): about
/// 0.14 to 0.2 a pixel, and a header.
fn expected_bytes(pixels: u64) -> u64 {
    1_500 + pixels * 18 / 100
}

/// The smallest long edge UXDuck sends, in pixels: smaller is no picture a
/// model can describe.
const SMALLEST_EDGE: u32 = 160;

/// Looks made at once. Each runs the capture helper decoding a camera's
/// video on this computer, which may be the one flying the vehicle, so
/// several chats looking at once can't load it (review, 2026-11-17).
const LOOKS_AT_ONCE: usize = 2;

/// The size a camera's `width` × `height` picture is sent at: even sides,
/// at most `LOOK_LONG_EDGE` on the long edge and never larger than the
/// camera's, and small enough that `frames` of them fit `max_bytes`.
///
/// # Errors
/// They would not fit even at the smallest size, in words.
pub fn look_size(
    width: u32,
    height: u32,
    frames: u32,
    max_bytes: Option<u64>,
) -> Result<(u32, u32), String> {
    let (width, height) = (width.max(2), height.max(2));
    let long = width.max(height);
    let mut edge = long.min(LOOK_LONG_EDGE);
    let size = |edge: u32| {
        let scale = f64::from(edge) / f64::from(long);
        let even = |side: u32| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a side of a picture, scaled down"
            )]
            let scaled = (f64::from(side) * scale).round() as u32;
            (scaled.max(2) / 2) * 2
        };
        (even(width), even(height))
    };
    if let Some(max_bytes) = max_bytes {
        let fits = |edge: u32| {
            let (w, h) = size(edge);
            expected_bytes(u64::from(w) * u64::from(h)) * u64::from(frames) <= max_bytes
        };
        while edge > SMALLEST_EDGE && !fits(edge) {
            edge = (edge * 9 / 10).max(SMALLEST_EDGE);
        }
        if !fits(edge) {
            return Err(format!(
                "{frames} {} would not fit in what this chat's AI model takes in one request, \
                 {} KB more. Ask for fewer frames, or start a new chat.",
                if frames == 1 { "frame" } else { "frames" },
                max_bytes / 1024
            ));
        }
    }
    Ok(size(edge))
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

impl Engine {
    /// The camera a look is through: the one named, or the vehicle's only
    /// one. Every miss says which cameras there are.
    pub(crate) fn look_source(&self, params: &VideoFrameParams) -> Result<VideoSource, ApiError> {
        let named = |sources: &[&VideoSource]| {
            sources
                .iter()
                .map(|s| format!("{} ({})", s.name, s.id))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if let Some(id) = &params.source_id {
            let source = self.state.video_sources.get(id).ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("No camera {id}. See video/list."),
                )
            })?;
            if let Some(vehicle) = &params.vehicle_id
                && source.vehicle_id.as_ref() != Some(vehicle)
            {
                return Err(invalid(format!(
                    "The camera {} is not {vehicle}'s.",
                    source.name
                )));
            }
            return Ok(source.clone());
        }
        let Some(vehicle) = &params.vehicle_id else {
            let all: Vec<&VideoSource> = self.state.video_sources.values().collect();
            return Err(invalid(if all.is_empty() {
                "There are no cameras. Add one on the vehicle's page.".to_owned()
            } else {
                format!(
                    "Name the vehicle or the camera. The cameras are {}.",
                    named(&all)
                )
            }));
        };
        let theirs: Vec<&VideoSource> = self
            .state
            .video_sources
            .values()
            .filter(|s| s.vehicle_id.as_ref() == Some(vehicle))
            .collect();
        match theirs.as_slice() {
            [] => Err(ApiError::new(
                ErrorKind::NotFound,
                format!("The vehicle {vehicle} has no camera. Add one on its page."),
            )),
            [only] => Ok((*only).clone()),
            several => Err(invalid(format!(
                "The vehicle has {} cameras. Name one with sourceId: {}.",
                several.len(),
                named(several)
            ))),
        }
    }

    /// `video/frame`: checked here, answered when the video host reports.
    #[allow(
        clippy::too_many_lines,
        reason = "each check on a look, in the order a person reads them"
    )]
    pub(crate) fn video_frame(
        &mut self,
        connection: ConnectionId,
        params: &VideoFrameParams,
    ) -> Result<Value, ApiError> {
        let operator = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
            .is_some_and(super::Admitted::is_operator);
        if !operator && self.state.settings.agent_cameras == AgentCameras::Never {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                format!(
                    "The assistant may not see through cameras on this computer. A person can \
                     change this in {CAMERAS_SETTING}."
                ),
            ));
        }
        let asked = asked(params)?;
        let source = self.look_source(params)?;
        let (frames, kept) = frames_the_camera_sends(&asked, source.frame_rate, &source.name);
        let max_bytes = params.max_bytes.unwrap_or(LOOK_REPLY_BYTES);
        let held = (max_bytes > LOOK_REPLY_BYTES).then(|| {
            format!(
                "Kept to {} MB of pictures, not the {} MB asked for: UXDuck holds a look's \
                 pictures in memory while it answers.",
                LOOK_REPLY_BYTES / (1024 * 1024),
                max_bytes.div_ceil(1024 * 1024)
            )
        });
        let max_bytes = max_bytes.min(LOOK_REPLY_BYTES);
        let kept = [kept, held]
            .into_iter()
            .flatten()
            .reduce(|a, b| format!("{a} {b}"));
        // A look from a chat was held for a person to allow before it got
        // here (approvals.rs). One outside a chat, such as an MCP client's,
        // has nobody to ask.
        if !operator
            && self.state.settings.agent_cameras == AgentCameras::Ask
            && !self.running_approved
        {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                format!(
                    "Each look through a camera is allowed by a person in the UXDuck chat that \
                     asked for it, and this request is not from a chat. A person can choose \
                     Always allow in {CAMERAS_SETTING}."
                ),
            ));
        }
        if self.video.looks.len() >= LOOKS_AT_ONCE {
            return Err(invalid(format!(
                "UXDuck is already making {LOOKS_AT_ONCE} looks through cameras, as many as it \
                 makes at once on this computer. Ask again when one has finished."
            )));
        }
        if self.video.port.is_none() {
            return Err(invalid(
                "Camera images need UXDuck Desktop on this computer.",
            ));
        }
        if source.state == VideoState::Unsupported {
            return Err(invalid(format!(
                "UXDuck can't decode {}'s video{}.",
                source.name,
                source
                    .state_detail
                    .as_deref()
                    .map(|why| format!(": {why}"))
                    .unwrap_or_default()
            )));
        }
        if source.capture.as_ref().is_some_and(|c| !c.enabled) {
            return Err(invalid(format!(
                "{} is turned off. A person turns it on in its camera settings.",
                source.name
            )));
        }
        let Some((_, request_id)) = self.replying_to.take() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video/frame is answered as a request",
            ));
        };
        let id = format!("lk{}", &self.random.token()[..16]);
        let order = LookOrder {
            id: id.clone(),
            source_id: source.id.clone(),
            camera: source.name.clone(),
            frames,
            seconds: asked.seconds,
            max_bytes: Some(max_bytes),
        };
        let asked = self.video.port.as_mut().map_or_else(
            || Err("Camera images need UXDuck Desktop.".to_owned()),
            |port| port.look(order),
        );
        if let Err(why) = asked {
            self.replying_to = Some((connection, request_id));
            return Err(invalid(why));
        }
        self.video.looks.insert(
            id,
            Waiting {
                connection,
                request_id,
                source_id: source.id,
                max_bytes: Some(max_bytes),
                kept,
            },
        );
        self.review_video_demand();
        Ok(Value::Null)
    }

    /// Stops the looks a cancelled request or a closed connection was
    /// waiting for, so the camera and the capture helper are let go. Nobody
    /// waits for their answer.
    pub(crate) fn stop_looks(&mut self, connection: ConnectionId, request_id: Option<&Value>) {
        let stopped: Vec<String> = self
            .video
            .looks
            .iter()
            .filter(|(_, w)| {
                w.connection == connection && request_id.is_none_or(|id| w.request_id == *id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if stopped.is_empty() {
            return;
        }
        for id in stopped {
            self.video.looks.remove(&id);
            if let Some(port) = self.video.port.as_mut() {
                port.stop_look(&id);
            }
        }
        self.review_video_demand();
    }

    /// The video host's answer to a look.
    pub fn video_looked(&mut self, id: &str, result: Result<Looked, String>) {
        let Some(waiting) = self.video.looks.remove(id) else {
            return;
        };
        self.review_video_demand();
        let source = self.state.video_sources.get(&waiting.source_id).cloned();
        let answer = result.and_then(|looked| {
            let source = source.ok_or("The camera was removed while UXDuck looked.")?;
            look_result(&source, looked, waiting.max_bytes, waiting.kept)
        });
        match answer {
            Ok(result) => self.send_look(waiting.connection, &waiting.request_id, &result),
            Err(why) => {
                let frame = crate::rpc::router::error(&waiting.request_id, &invalid(why));
                self.send(waiting.connection, &frame);
            }
        }
    }

    /// A look's answer, serialised once from its frames' base64 and never
    /// copied into a JSON value. The trace and MCP's analytics see it with
    /// each frame's JPEG left out.
    fn send_look(
        &mut self,
        connection: ConnectionId,
        request_id: &Value,
        result: &VideoFrameResult,
    ) {
        let text = serde_json::to_string(&json_rpc::Reply {
            jsonrpc: "2.0",
            id: request_id,
            result,
        })
        .unwrap_or_default();
        let mut traced = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "sourceId": result.source_id,
                "camera": result.camera,
                "frames": [],
            },
        });
        traced["result"]["frames"] = result
            .frames
            .iter()
            .map(|f| {
                json!({
                    "offsetS": f.offset_s,
                    "atUs": f.at_us,
                    "width": f.width,
                    "height": f.height,
                    "jpegBase64": format!("({} characters of JPEG left out)", f.jpeg_base64.len()),
                })
            })
            .collect();
        self.send_text(connection, text, &traced);
    }
}

fn look_result(
    source: &VideoSource,
    looked: Looked,
    max_bytes: Option<u64>,
    kept: Option<String>,
) -> Result<VideoFrameResult, String> {
    let Some(first) = looked.frames.first() else {
        return Err(format!(
            "UXDuck could not make a picture from {}.",
            source.name
        ));
    };
    let bytes: u64 = looked.frames.iter().map(|f| f.jpeg_bytes).sum();
    if let Some(max_bytes) = max_bytes
        && bytes > max_bytes
    {
        return Err(format!(
            "The {} {} came to {} KB, more than the {} KB this chat's AI model takes in one \
             request. Ask for fewer frames.",
            looked.frames.len(),
            if looked.frames.len() == 1 {
                "frame"
            } else {
                "frames"
            },
            bytes.div_ceil(1024),
            max_bytes / 1024
        ));
    }
    let (width, height) = (first.width, first.height);
    let scaled = match (looked.camera_width, looked.camera_height) {
        (Some(w), Some(h)) if (w, h) != (width, height) => Some(format!(
            "Scaled from the camera's {w} × {h} to {width} × {height}."
        )),
        _ => None,
    };
    let note = [kept, scaled].into_iter().flatten().collect::<Vec<_>>();
    Ok(VideoFrameResult {
        source_id: source.id.clone(),
        camera: source.name.clone(),
        vehicle_id: source.vehicle_id.clone(),
        camera_width: looked.camera_width,
        camera_height: looked.camera_height,
        frames: looked
            .frames
            .into_iter()
            .map(|f| {
                #[allow(clippy::cast_precision_loss, reason = "seconds of a short clip")]
                let offset_s = f.offset_us as f64 / 1e6;
                VideoFrame {
                    offset_s,
                    at_us: f.at_us,
                    width: f.width,
                    height: f.height,
                    jpeg_base64: f.jpeg_base64,
                }
            })
            .collect(),
        note: (!note.is_empty()).then(|| note.join(" ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_at_most_1024_on_its_long_edge_and_never_enlarged() {
        assert_eq!(look_size(1920, 1080, 1, None).unwrap(), (1024, 576));
        assert_eq!(look_size(1080, 1920, 1, None).unwrap(), (576, 1024));
        assert_eq!(look_size(640, 480, 1, None).unwrap(), (640, 480));
        assert_eq!(look_size(721, 405, 1, None).unwrap(), (720, 404));
    }

    #[test]
    fn frames_are_made_smaller_to_fit_a_models_request_and_refused_when_they_cannot() {
        let (w, h) = look_size(1920, 1080, 8, Some(100 * 1024)).unwrap();
        assert!((SMALLEST_EDGE..1024).contains(&w), "{w}");
        assert!(expected_bytes(u64::from(w) * u64::from(h)) * 8 <= 100 * 1024);
        let refused = look_size(1920, 1080, 8, Some(8 * 1024)).unwrap_err();
        assert!(
            refused.contains("fewer frames") && refused.contains("8 KB"),
            "{refused}"
        );
        assert!(!refused.contains(';'));
    }
}
