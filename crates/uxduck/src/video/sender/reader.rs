//! A camera read from `MediaMTX` over localhost RTSP (ADR-0064 W6, W11), on
//! TCP, as the relay or the camera's own path serves it. Each picture keeps
//! the camera's RTP time and its capture time from `MediaMTX`'s sender reports,
//! which say when each picture was taken (S355), so the sender's own
//! reports carry the camera's times. Keyframes carry their parameter sets.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use uxduck_media::nal::{Codec, h264, split_annex_b};
use uxduck_media::rtcp::{SenderReport, sender_report};
use uxduck_media::rtp::{Depacketizer, Picture, parse};

use super::super::rtsp::{self, Frame as Answer, Payload, Session};

/// Waits between attempts to read the camera; the last repeats.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
/// Seconds from 1900, NTP's epoch, to 1970.
const NTP_TO_UNIX_S: u64 = 2_208_988_800;

/// One picture as the camera made it.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Annex B, with SPS and PPS before a keyframe's slices.
    pub annex_b: Vec<u8>,
    /// 90 kHz, the camera's own, never wrapping.
    pub rtp: u64,
    /// When it was taken, by the camera computer's clock.
    pub captured: SystemTime,
    pub key: bool,
}

/// Reads `url` until `pictures` closes, trying again after each failure.
pub async fn read(path: String, url: String, pictures: mpsc::Sender<(String, Frame)>) {
    let mut attempt = 0;
    let mut said = String::new();
    loop {
        let error = match play(&path, &url, &pictures).await {
            Ok(()) => return,
            Err(error) => error,
        };
        if pictures.is_closed() {
            return;
        }
        if error != said {
            tracing::warn!(%path, %error, "cannot read the camera from MediaMTX, trying again");
            said = error;
        }
        tokio::time::sleep(BACKOFF[attempt.min(BACKOFF.len() - 1)]).await;
        attempt += 1;
    }
}

/// One RTSP session: DESCRIBE, SETUP on TCP, PLAY, and the pictures.
/// `Ok` once `pictures` closes.
async fn play(
    path: &str,
    url: &str,
    pictures: &mpsc::Sender<(String, Frame)>,
) -> Result<(), String> {
    let address = rtsp::address(url)?;
    let mut s = Session::connect(address.clone()).await?;
    let (headers, body) = s
        .ask(
            "DESCRIBE",
            &address.url,
            &[("Accept", "application/sdp".into())],
        )
        .await?;
    let base = headers
        .get("content-base")
        .or_else(|| headers.get("content-location"))
        .cloned()
        .unwrap_or_else(|| address.url.clone());
    let sdp = String::from_utf8_lossy(&body);
    let video = h264_track(&sdp, &base)?;
    let (headers, _) = s
        .ask(
            "SETUP",
            &video.control,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1".into())],
        )
        .await?;
    let session = headers.get("session").ok_or("SETUP gave no session")?;
    let timeout = session
        .split(';')
        .find_map(|p| p.trim().strip_prefix("timeout="))
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(60);
    s.id = Some(session.split(';').next().unwrap_or(session).to_owned());
    s.ask("PLAY", &base, &[("Range", "npt=0.000-".into())])
        .await?;
    tracing::info!(%path, "reading the camera from MediaMTX");
    let mut keepalive = tokio::time::interval(Duration::from_secs((timeout / 2).clamp(5, 25)));
    keepalive.reset();
    let mut pictures_of = Pictures::new(&video.parameter_sets);
    let mut depacketizer = Depacketizer::new(Codec::H264);
    loop {
        tokio::select! {
            frame = s.frame() => match frame? {
                Answer::Interleaved { channel: 0, data } => {
                    let Some((header, payload)) = parse(&data) else {
                        continue;
                    };
                    for picture in depacketizer.push(&header, payload) {
                        let frame = pictures_of.frame(&picture);
                        if pictures.send((path.to_owned(), frame)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Answer::Interleaved { channel: 1, data } => {
                    if let Some(report) = sender_report(&data) {
                        pictures_of.report = Some(report);
                    }
                }
                _ => {}
            },
            _ = keepalive.tick() => {
                s.ping(&base).await?;
            }
            () = pictures.closed() => return Ok(()),
        }
    }
}

/// Turns the depacketized pictures into frames: their parameter sets,
/// their RTP time unwrapped and their capture time.
struct Pictures {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    report: Option<SenderReport>,
    last: Option<(u32, u64)>,
}

impl Pictures {
    fn new(parameter_sets: &[Vec<u8>]) -> Self {
        let mut pictures = Self {
            sps: None,
            pps: None,
            report: None,
            last: None,
        };
        for set in parameter_sets {
            pictures.keep(set);
        }
        pictures
    }

    fn keep(&mut self, nal: &[u8]) {
        match nal.first().map(|b| b & 0x1f) {
            Some(h264::SPS) => self.sps = Some(nal.to_vec()),
            Some(h264::PPS) => self.pps = Some(nal.to_vec()),
            _ => {}
        }
    }

    fn frame(&mut self, picture: &Picture) -> Frame {
        let kinds: Vec<u8> = split_annex_b(&picture.annex_b)
            .into_iter()
            .filter_map(|nal| {
                self.keep(nal);
                nal.first().map(|b| b & 0x1f)
            })
            .collect();
        let key = kinds.contains(&h264::IDR);
        let mut annex_b = Vec::with_capacity(picture.annex_b.len() + 64);
        if key {
            for (kind, set) in [(h264::SPS, &self.sps), (h264::PPS, &self.pps)] {
                if let Some(set) = set
                    && !kinds.contains(&kind)
                {
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(set);
                }
            }
        }
        annex_b.extend_from_slice(&picture.annex_b);
        let rtp = match self.last {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "the step between two times, signed"
            )]
            Some((last, unwrapped)) => unwrapped
                .wrapping_add_signed(i64::from(picture.timestamp.wrapping_sub(last) as i32)),
            None => u64::from(picture.timestamp),
        };
        self.last = Some((picture.timestamp, rtp));
        Frame {
            annex_b,
            rtp,
            captured: self.captured(picture.timestamp),
            key,
        }
    }

    /// The capture time the last sender report gives this RTP time, or now
    /// before the first report.
    fn captured(&self, timestamp: u32) -> SystemTime {
        let Some(report) = self.report else {
            return SystemTime::now();
        };
        let seconds = (report.ntp >> 32).saturating_sub(NTP_TO_UNIX_S);
        let nanos = ((report.ntp & 0xffff_ffff) * 1_000_000_000) >> 32;
        let at = UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_nanos(nanos);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "the step between two times, signed"
        )]
        let ticks = i64::from(timestamp.wrapping_sub(report.rtp) as i32);
        let step = Duration::from_micros(ticks.unsigned_abs() * 1000 / 90);
        if ticks >= 0 { at + step } else { at - step }
    }
}

/// The path's H.264 track. A Raspberry Pi camera with a secondary encode
/// also carries a track `MediaMTX` calls Generic, which is left alone.
fn h264_track(sdp: &str, base: &str) -> Result<rtsp::Video, String> {
    rtsp::tracks(sdp, base)?
        .into_iter()
        .find(|track| track.payload == Payload::Video(Codec::H264))
        .ok_or_else(|| "the camera's video is not H.264, which is all the sender sends".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_h264_track_is_read_beside_a_secondary_encodes_generic_track() {
        let sdp = "v=0
o=- 0 0 IN IP4 127.0.0.1
s=Stream
t=0 0
            m=application 0 RTP/AVP 96
a=control:trackID=0
a=rtpmap:96 rpicamera_secondary/90000
            m=video 0 RTP/AVP 97
a=control:trackID=1
a=rtpmap:97 H264/90000
            a=fmtp:97 packetization-mode=1; sprop-parameter-sets=Z0LAH9oBQBbsBEAAAAMAQAAADyPGDKg=,aM48gA==
";
        let video = h264_track(sdp, "rtsp://127.0.0.1:8554/front/").unwrap();
        assert_eq!(video.control, "rtsp://127.0.0.1:8554/front/trackID=1");
        assert_eq!(video.parameter_sets.len(), 2);
        let generic_only = "v=0
m=application 0 RTP/AVP 96
a=control:trackID=0
a=rtpmap:96 rpicamera_secondary/90000
";
        assert!(h264_track(generic_only, "rtsp://127.0.0.1:8554/front/").is_err());
    }

    #[test]
    fn a_keyframe_gets_its_parameter_sets_and_times_unwrap_and_date_by_the_report() {
        let sps = vec![0x67, 0x42, 0xc0, 0x1f];
        let pps = vec![0x68, 0xce, 0x3c, 0x80];
        let mut pictures = Pictures::new(&[sps.clone(), pps.clone()]);
        let idr = Picture {
            annex_b: vec![0, 0, 0, 1, 0x65, 0x88, 0x84],
            timestamp: u32::MAX - 1499,
            damaged: false,
        };
        let frame = pictures.frame(&idr);
        assert!(frame.key);
        assert_eq!(
            frame.annex_b,
            [
                &[0, 0, 0, 1][..],
                &sps,
                &[0, 0, 0, 1],
                &pps,
                &[0, 0, 0, 1, 0x65, 0x88, 0x84]
            ]
            .concat()
        );
        // A report saying RTP time 0 was taken at 1,000,000,000 s past 1970.
        pictures.report = Some(SenderReport {
            ssrc: 1,
            ntp: (1_000_000_000 + NTP_TO_UNIX_S) << 32,
            rtp: 0,
        });
        let next = pictures.frame(&Picture {
            annex_b: vec![0, 0, 0, 1, 0x41, 0x9a],
            timestamp: 1500,
            damaged: false,
        });
        assert!(!next.key);
        assert_eq!(next.annex_b, [0, 0, 0, 1, 0x41, 0x9a]);
        // Across the wrap, 3000 ticks later.
        assert_eq!(next.rtp, u64::from(u32::MAX - 1499) + 3000);
        assert_eq!(
            next.captured,
            UNIX_EPOCH + Duration::from_secs(1_000_000_000) + Duration::from_micros(16_666)
        );
    }
}
