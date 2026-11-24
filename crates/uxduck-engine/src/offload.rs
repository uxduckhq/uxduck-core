//! Move or copy flights to a disk (ADR-0062): each flight written as one
//! `.uxflight` file, an ordinary zip, into a folder on an external disk, by
//! hand from the Logbook. The host writes, reads back and renames each file
//! into place off the engine's task ([`System::offload`]). Move then deletes
//! the video only these flights use, never a telemetry log, and keeps where
//! in the file each recording now is, so it still plays from the disk.
//!
//! A `.uxflight` opened (`replay/open`) or found in a Log folder is added to
//! the Logbook by reference: nothing is copied, as an added video stays
//! where it is, and the flight says "Not plugged in" while its disk is not.
//!
//! [`System::offload`]: crate::runtime::System::offload

use std::collections::{BTreeMap, BTreeSet};

use uxduck_contracts::dto::flight::{
    FlightOffload, FlightOffloadParams, FlightOffloadResult, place_of, place_of_folder,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::flight::{Flight, FlightCopy, FlightEnd};
use uxduck_core::domain::video::{PackedVideo, RecordingState, VideoRecording};
use uxduck_core::events::{Actor, Change};

use crate::Engine;
use crate::runtime::{OffloadFlight, OffloadJob, OffloadVideo, ReplayRead, VideoAt};

/// Whether a file is in this folder or below it.
pub(crate) fn inside(folder: &str, path: &str) -> bool {
    path.strip_prefix(folder.trim_end_matches(['/', '\\']))
        .is_some_and(|rest| rest.starts_with(['/', '\\']))
}

/// The flight's telemetry in a `.uxflight`.
pub const TELEMETRY: &str = "telemetry.tlog";

/// Move or copy under way, and the flights whose last one failed.
#[derive(Debug, Default)]
pub(crate) struct Offloads {
    jobs: u64,
    /// By flight id.
    flights: BTreeMap<String, FlightOffload>,
    running: BTreeMap<u64, Job>,
    /// `.uxflight` files found in Log folders that could not be added, so
    /// each is tried once.
    unreadable: BTreeSet<String>,
}

/// A recording's places in its flight's file: the recording, the original
/// and the playable copy.
type Packed = (String, String, String);

#[derive(Debug)]
struct Job {
    moving: bool,
    /// The flights in the order written, each with its recordings' places
    /// in its file: the recording, the original and the playable copy.
    flights: Vec<(String, Vec<Packed>)>,
    /// Flights written and read back, with their files.
    written: BTreeMap<String, String>,
}

fn actor() -> Actor {
    Actor {
        client_id: "offload".into(),
        operator_id: None,
        identity: None,
    }
}

fn invalid(why: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, why)
}

/// A recording's place in a file: the original, and the copy every player
/// plays, unless the original is one.
fn names_in_file(file_name: &str) -> (String, String) {
    let entry = format!("video/{file_name}");
    let path = std::path::Path::new(file_name);
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ts"))
    {
        let stem = path.file_stem().map_or_else(
            || file_name.to_owned(),
            |s| s.to_string_lossy().into_owned(),
        );
        (entry, format!("video/{stem}.mp4"))
    } else {
        (entry.clone(), entry)
    }
}

/// What a recording takes in a file: a `.ts` goes with its MP4 copy, about
/// as large again.
fn bytes_in_file(video: &OffloadVideo) -> u64 {
    if video.entry == video.playable {
        video.recording.bytes
    } else {
        video.recording.bytes.saturating_mul(2)
    }
}

/// `stem`, or `stem 2`, `stem 3` and on when its file's name is taken, by
/// name in lower case, as exFAT, APFS and NTFS compare names. The name is
/// taken from then on.
fn free_stem(taken: &mut BTreeSet<String>, stem: &str) -> String {
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            stem.to_owned()
        } else {
            format!("{stem} {n}")
        };
        if taken.insert(format!("{candidate}.uxflight").to_lowercase()) {
            return candidate;
        }
        n += 1;
    }
}

/// `video/<stem> <n>.<ext>` for the `n`th video of one name.
fn numbered(entry: &str, n: u32) -> String {
    let path = std::path::Path::new(entry);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    format!("video/{stem} {n}{ext}")
}

impl Engine {
    /// The recordings the Logbook counts as a flight's video: added to it,
    /// or of its vehicle over its time.
    pub(crate) fn flight_videos(&self, flight: &Flight) -> Vec<VideoRecording> {
        let mut videos: Vec<VideoRecording> = self
            .state
            .video_recordings
            .values()
            .filter(|r| self.is_flight_video(flight, r))
            .cloned()
            .collect();
        videos.sort_by_key(|r| (r.started_at_us, r.id.clone()));
        videos
    }

    /// Whether the Logbook counts this recording as the flight's video.
    pub(crate) fn is_flight_video(&self, flight: &Flight, recording: &VideoRecording) -> bool {
        if recording.state == RecordingState::Recording || recording.state == RecordingState::Failed
        {
            return false;
        }
        let end = flight.ended_at_us.unwrap_or_else(|| self.clock.now_us());
        let r_end = recording.ended_at_us.unwrap_or_else(|| {
            recording
                .started_at_us
                .saturating_add(recording.duration_us)
        });
        recording.flight_id.as_deref() == Some(flight.id.as_str())
            || (recording.vehicle_id.as_deref() == Some(flight.vehicle_id.as_str())
                && recording.started_at_us < end
                && flight.armed_at_us < r_end)
    }

    /// Each Log folder's count of Logbook entries with a copy in it or
    /// below it (S379), so a disk of `.uxflight` files does not read 0.
    pub(crate) fn count_folder_entries(
        &self,
        found: &mut uxduck_contracts::dto::replay::FoundFlightsResult,
    ) {
        for folder in &mut found.folders {
            let entries = self
                .state
                .flights
                .values()
                .filter(|f| f.copies.iter().any(|c| inside(&folder.path, &c.path)))
                .count();
            folder.entries = u64::try_from(entries).unwrap_or(u64::MAX);
        }
    }

    /// The videos of a flight as its file holds them, each named once.
    fn videos_for_file(&self, flight: &Flight) -> Vec<OffloadVideo> {
        let mut taken = BTreeSet::new();
        let mut videos = Vec::new();
        for recording in self.flight_videos(flight) {
            let from = match (&recording.packed, &recording.alignment) {
                (Some(packed), _) => VideoAt::Packed(packed.clone()),
                (None, Some(_)) => VideoAt::Added {
                    path: recording.file(),
                },
                (None, None) => VideoAt::Media {
                    file: recording.file(),
                },
            };
            let (entry, playable) = match &from {
                VideoAt::Packed(p) => (p.entry.clone(), p.playable.clone()),
                VideoAt::Media { file } => names_in_file(file),
                // Kept as it was, under a name exFAT and NTFS take when the
                // file is unzipped: a Mac's file may have `:` or `?` in it.
                VideoAt::Added { path } => {
                    let path = std::path::Path::new(path);
                    let stem = path.file_stem().map_or_else(
                        || recording.id.clone(),
                        |s| s.to_string_lossy().into_owned(),
                    );
                    let ext = path
                        .extension()
                        .map(|e| format!(".{}", crate::photos::file_safe(&e.to_string_lossy())))
                        .unwrap_or_default();
                    let entry = format!("video/{}{ext}", crate::photos::file_safe(&stem));
                    (entry.clone(), entry)
                }
            };
            // Two added videos may share a name: the second is " 2".
            let mut unique = entry.clone();
            let mut n = 1;
            // Told apart as unzipped onto a disk that ignores case.
            while taken.contains(&unique.to_lowercase()) {
                n += 1;
                unique = numbered(&entry, n);
            }
            let playable = if playable == entry {
                unique.clone()
            } else if n > 1 {
                numbered(&playable, n)
            } else {
                playable
            };
            taken.insert(unique.to_lowercase());
            taken.insert(playable.to_lowercase());
            videos.push(OffloadVideo {
                recording,
                entry: unique,
                playable,
                from,
            });
        }
        videos
    }

    /// The flight's telemetry for its file: its connection's log over its
    /// replay, or the file it was opened from.
    fn telemetry_for_file(&self, flight: &Flight) -> Option<ReplayRead> {
        match &flight.link_id {
            Some(link_id) => {
                let (from_us, to_us) = self.armed_span(flight);
                let own = ReplayRead::Link {
                    link_id: link_id.clone(),
                    from_us,
                    to_us,
                    start: flight.tlog.as_ref().map(|t| (t.file.clone(), t.offset)),
                };
                Some(self.with_other_links(flight, own, (from_us, to_us)))
            }
            None => self.present_copy(flight).map(|path| ReplayRead::Packed {
                path,
                entry: TELEMETRY.to_owned(),
            }),
        }
    }

    /// The first of a flight's files that is there now, or that this host
    /// cannot tell is not.
    pub(crate) fn present_copy(&self, flight: &Flight) -> Option<String> {
        flight
            .copies
            .iter()
            .find(|c| self.system.file_present(&c.path) != Some(false))
            .map(|c| c.path.clone())
    }

    /// The file's name before `.uxflight`: its name or its vehicle's, and
    /// when it armed in this computer's time.
    fn file_stem(&self, flight: &Flight) -> String {
        let offset = self
            .video
            .port
            .as_ref()
            .map_or(0, |port| port.utc_offset_minutes(flight.armed_at_us));
        let at = uxduck_media::photo::Civil::at(flight.armed_at_us, offset);
        let name = flight.name.as_deref().unwrap_or(&flight.vehicle_name);
        format!(
            "{} {:04}-{:02}-{:02} {:02}.{:02}",
            crate::photos::file_safe(name),
            at.year,
            at.month,
            at.day,
            at.hour,
            at.minute
        )
    }

    /// `flight/offload` (ADR-0062): checks and says what writing these
    /// flights to `folder` would do, or starts it.
    ///
    /// # Errors
    /// A flight that is not there, is still armed or is being written
    /// already, a Move of the flight being replayed, a folder that is not
    /// there or cannot be written, and a disk without room, in the numbers
    /// measured.
    #[allow(clippy::too_many_lines, reason = "one check, then one start")]
    pub(crate) fn offload_flights(
        &mut self,
        params: &FlightOffloadParams,
    ) -> Result<FlightOffloadResult, ApiError> {
        let mut ids: Vec<String> = Vec::new();
        for id in &params.ids {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        if ids.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "Choose the flights to move or copy.",
            ));
        }
        let replaying = self.replay_flight_ids();
        let mut flights = Vec::new();
        for id in &ids {
            let flight = self
                .state
                .flights
                .get(id)
                .cloned()
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
            let name = flight
                .name
                .clone()
                .unwrap_or_else(|| flight.vehicle_name.clone());
            if flight.end == FlightEnd::Open {
                return Err(invalid(format!(
                    "{name} is still armed. Move or copy it once the flight has ended."
                )));
            }
            if self
                .offloads
                .flights
                .get(id)
                .is_some_and(|o| o.error.is_none())
            {
                return Err(invalid(format!(
                    "{name} is being written to a disk already."
                )));
            }
            if params.moving && replaying.contains(id) {
                return Err(invalid(format!(
                    "{name} is open in a replay. Close the replay to move its video."
                )));
            }
            flights.push(flight);
        }
        let target = self
            .system
            .offload_target(&params.folder)
            .map_err(invalid)?;
        let place = place_of_folder(&target.folder);
        let id_set: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        let freed: Vec<VideoRecording> = if params.moving {
            self.freed_recordings(&id_set)
        } else {
            Vec::new()
        };
        let mut job_flights = Vec::new();
        // What the files take: at least the recordings as they are, and at
        // most with each camera recording's MP4 copy as large again. The
        // copy is made as it is written, so its size is not known before.
        // The telemetry adds what its logs measure: a link's at most what
        // they hold from the flight's start, as later flights may follow.
        let (mut video_bytes, mut most_bytes, mut sure_bytes) = (0u64, 0u64, 0u64);
        let mut videos = 0u32;
        let mut theirs: BTreeMap<String, VideoRecording> = BTreeMap::new();
        let mut taken = target.names.clone();
        let (mut too_large, mut may_not_fit) = (Vec::new(), Vec::new());
        let mut unplugged: Option<String> = None;
        for flight in &flights {
            let in_file = self.videos_for_file(flight);
            let telemetry = self.telemetry_for_file(flight);
            let (mut least, mut most) = telemetry
                .as_ref()
                .and_then(|read| self.system.telemetry_bytes(read))
                .unwrap_or((0, 0));
            let mut flight_video = 0u64;
            for video in &in_file {
                flight_video = flight_video.saturating_add(bytes_in_file(video));
                most = most.saturating_add(bytes_in_file(video));
                least = least.saturating_add(video.recording.bytes);
                videos += 1;
                theirs.insert(video.recording.id.clone(), video.recording.clone());
            }
            // A Copy then a Move, or a second Copy, to the same folder keeps
            // the file there rather than writing the flight again.
            let reuse = flight.copies.iter().map(|c| c.path.clone()).find(|path| {
                std::path::Path::new(path).parent() == Some(std::path::Path::new(&target.folder))
                    && self.system.file_present(path) != Some(false)
            });
            let stem = match &reuse {
                Some(path) => std::path::Path::new(path).file_stem().map_or_else(
                    || self.file_stem(flight),
                    |s| s.to_string_lossy().into_owned(),
                ),
                None => free_stem(&mut taken, &self.file_stem(flight)),
            };
            if reuse.is_none()
                && let Some(away) = in_file.iter().find_map(|v| match &v.from {
                    VideoAt::Packed(packed)
                        if self.system.file_present(&packed.path) == Some(false) =>
                    {
                        Some(place_of(&packed.path))
                    }
                    _ => None,
                })
            {
                let name = flight.name.as_deref().unwrap_or(&flight.vehicle_name);
                unplugged.get_or_insert(format!(
                    "{name}'s video is on {away}, which is not plugged in. Plug it in to move or copy it."
                ));
            }
            if reuse.is_none() {
                video_bytes = video_bytes.saturating_add(flight_video);
                most_bytes = most_bytes.saturating_add(most);
                sure_bytes = sure_bytes.saturating_add(least);
                if let Some((largest, _)) = &target.max_file {
                    let name = flight.name.as_deref().unwrap_or(&flight.vehicle_name);
                    if least > *largest {
                        too_large
                            .push(format!("{name} ({})", crate::sync::size_words(least, true)));
                    } else if most > *largest {
                        may_not_fit.push(name.to_owned());
                    }
                }
            }
            job_flights.push(OffloadFlight {
                flight: Flight {
                    copies: Vec::new(),
                    autopilot_log: None,
                    ..flight.clone()
                },
                stem,
                telemetry,
                videos: in_file,
                reuse,
            });
        }
        let freed_ids: BTreeSet<&str> = freed.iter().map(|r| r.id.as_str()).collect();
        let kept: Vec<&VideoRecording> = theirs
            .values()
            .filter(|r| r.packed.is_none() && !freed_ids.contains(r.id.as_str()))
            .collect();
        let format_words = |(largest, format): &(u64, String)| {
            format!(
                "{place} is formatted as {format}, which keeps files of {} at most",
                crate::sync::size_words(*largest, true)
            )
        };
        let refused = match (&target.max_file, target.free_bytes) {
            _ if unplugged.is_some() => unplugged,
            (Some(max_file), _) if !too_large.is_empty() => Some(format!(
                "{}, and these flights' files need more: {}. Leave them out, or choose a disk formatted as exFAT, APFS or NTFS.",
                format_words(max_file),
                too_large.join(", ")
            )),
            (_, Some(free)) if free < sure_bytes => Some(format!(
                "{place} has {} free, and these flights' files need {} at least.",
                crate::sync::size_words(free, false),
                crate::sync::size_words(sure_bytes, true)
            )),
            _ => None,
        };
        // Allowed, and said: what may not fit, in the numbers measured.
        let mut warnings = Vec::new();
        if let (Some(free), true) = (target.free_bytes, refused.is_none())
            && free < most_bytes
        {
            warnings.push(format!(
                "{place} has {} free. These flights' files take {} to {} with their telemetry logs and MP4 copies, so some may not fit. One that does not is not written, and nothing on this computer is deleted for it.",
                crate::sync::size_words(free, false),
                crate::sync::size_words(sure_bytes, true),
                crate::sync::size_words(most_bytes, true)
            ));
        }
        if let (Some(max_file), true) = (
            &target.max_file,
            refused.is_none() && !may_not_fit.is_empty(),
        ) {
            warnings.push(format!(
                "{}, so the file of {} may not fit with its telemetry log and MP4 copies. One that does not is not written.",
                format_words(max_file),
                may_not_fit.join(", ")
            ));
        }
        let result = FlightOffloadResult {
            folder: target.folder.clone(),
            place: place.clone(),
            adds_folder: !target.log_folder,
            files: job_flights
                .iter()
                .map(|f| format!("{}.uxflight", f.stem))
                .collect(),
            warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
            video_bytes,
            videos,
            freed_bytes: freed.iter().map(|r| r.bytes).sum(),
            kept_videos: if params.moving {
                u32::try_from(kept.len()).unwrap_or(u32::MAX)
            } else {
                0
            },
            kept_bytes: if params.moving {
                kept.iter().map(|r| r.bytes).sum()
            } else {
                0
            },
            free_bytes: target.free_bytes,
            refused,
        };
        if params.check {
            return Ok(result);
        }
        if let Some(refused) = &result.refused {
            return Err(invalid(refused.clone()));
        }
        self.offloads.jobs += 1;
        let job_id = self.offloads.jobs;
        let job = OffloadJob {
            id: job_id,
            folder: target.folder.clone(),
            // A Log folder from now, so its flights are listed, and say
            // "Not plugged in" while its disk is not.
            add_folder: (!target.log_folder).then_some(target.deep),
            flights: job_flights,
        };
        let places = job
            .flights
            .iter()
            .map(|f| {
                (
                    f.flight.id.clone(),
                    f.videos
                        .iter()
                        .map(|v| (v.recording.id.clone(), v.entry.clone(), v.playable.clone()))
                        .collect(),
                )
            })
            .collect();
        for flight in &job.flights {
            self.offloads.flights.insert(
                flight.flight.id.clone(),
                FlightOffload {
                    to: target.folder.clone(),
                    place: place.clone(),
                    moving: params.moving,
                    done_bytes: 0,
                    total_bytes: flight.videos.iter().map(bytes_in_file).sum(),
                    error: None,
                },
            );
        }
        self.offloads.running.insert(
            job_id,
            Job {
                moving: params.moving,
                flights: places,
                written: BTreeMap::new(),
            },
        );
        if let Err(why) = self.system.offload(job) {
            self.offloads.running.remove(&job_id);
            for id in &ids {
                self.offloads.flights.remove(id);
            }
            return Err(invalid(why));
        }
        Ok(result)
    }

    /// How far the host has got with a flight's file (ADR-0062): bytes
    /// written and read back, of all it will.
    pub fn offload_progress(&mut self, job: u64, flight_id: &str, done: u64, total: u64) {
        if !self.offloads.running.contains_key(&job) {
            return;
        }
        if let Some(offload) = self.offloads.flights.get_mut(flight_id) {
            offload.done_bytes = done;
            offload.total_bytes = total;
        }
    }

    /// A flight's file is written and read back, at `written`, or why not.
    pub fn offload_written(&mut self, job: u64, flight_id: &str, written: Result<String, String>) {
        let Some(running) = self.offloads.running.get_mut(&job) else {
            return;
        };
        let path = match written {
            Ok(path) => path,
            Err(why) => {
                if let Some(offload) = self.offloads.flights.get_mut(flight_id) {
                    offload.error = Some(why);
                }
                return;
            }
        };
        running.written.insert(flight_id.to_owned(), path.clone());
        let moving = running.moving;
        self.add_copy(flight_id, &path);
        // A Copy is done. A Move frees space once every file is written.
        if !moving {
            self.offloads.flights.remove(flight_id);
        }
    }

    /// Adds a file to a flight's copies, once.
    fn add_copy(&mut self, flight_id: &str, path: &str) {
        let Some(mut flight) = self.state.flights.get(flight_id).cloned() else {
            return;
        };
        if flight.copies.iter().any(|c| c.path == path) {
            return;
        }
        flight.copies.push(FlightCopy {
            path: path.to_owned(),
        });
        flight.version += 1;
        self.commit_as(actor(), Change::FlightUpserted { flight });
    }

    /// Every file of a job is written or failed. A Move deletes the video
    /// only the written flights use, each recording then kept as where it is
    /// in the first file that holds it. A failed flight's video stays, as do
    /// the recordings it shares. Telemetry logs are never deleted.
    pub fn offload_done(&mut self, job: u64) {
        let Some(running) = self.offloads.running.remove(&job) else {
            return;
        };
        // One the host never finished says so, rather than seeming to run.
        for (flight_id, _) in &running.flights {
            if let Some(offload) = self.offloads.flights.get_mut(flight_id)
                && offload.error.is_none()
                && !running.written.contains_key(flight_id)
            {
                offload.error = Some("UXDuck stopped before it was written.".to_owned());
            }
        }
        if !running.moving {
            return;
        }
        let written: BTreeSet<&str> = running.written.keys().map(String::as_str).collect();
        let freed = self.freed_recordings(&written);
        let mut failed: Option<String> = None;
        for recording in freed {
            let Some(packed) = running.flights.iter().find_map(|(flight_id, videos)| {
                let path = running.written.get(flight_id)?;
                videos
                    .iter()
                    .find(|(id, ..)| *id == recording.id)
                    .map(|(_, entry, playable)| PackedVideo {
                        path: path.clone(),
                        entry: entry.clone(),
                        playable: playable.clone(),
                    })
            }) else {
                continue;
            };
            let deleted = match self.video.port.as_mut() {
                Some(port) => port.delete_recording(&recording.id, &recording.file()),
                None => Err("Recordings need UXDuck Desktop.".into()),
            };
            if let Err(why) = deleted {
                failed.get_or_insert_with(|| {
                    format!(
                        "Written to {}, but its video could not be deleted from this computer. {why}",
                        place_of(&packed.path)
                    )
                });
                continue;
            }
            let version = recording.version + 1;
            self.commit_as(
                actor(),
                Change::VideoRecordingUpserted {
                    recording: VideoRecording {
                        packed: Some(packed),
                        version,
                        ..recording
                    },
                },
            );
        }
        for flight_id in running.written.keys() {
            match &failed {
                Some(why) => {
                    if let Some(offload) = self.offloads.flights.get_mut(flight_id) {
                        offload.error = Some(why.clone());
                    }
                }
                None => {
                    self.offloads.flights.remove(flight_id);
                }
            }
        }
    }

    /// A `.uxflight` opened or found (ADR-0062): its flight, added to the
    /// Logbook by reference when it is not in it, or given this file. Its
    /// recordings play from the file. The flight's id.
    ///
    /// # Errors
    /// The file cannot be read, or its flight was deleted from this Logbook.
    pub(crate) fn adopt_flight_file(&mut self, path: &str) -> Result<String, ApiError> {
        let file = self.system.flight_file(path).map_err(invalid)?;
        let id = file.flight.id.clone();
        if self.state.flights.contains_key(&id) {
            self.add_copy(&id, path);
        } else {
            let flight = Flight {
                recorded_here: false,
                keep: false,
                left_account: false,
                link_id: None,
                tlog: None,
                tlog_end: None,
                other_links: Vec::new(),
                telemetry_gaps: None,
                log: uxduck_core::domain::flight::FlightLog::None,
                sync_video: false,
                copies: vec![FlightCopy {
                    path: path.to_owned(),
                }],
                version: 1,
                ..file.flight
            };
            self.commit_as(actor(), Change::FlightUpserted { flight });
            if !self.state.flights.contains_key(&id) {
                return Err(invalid(
                    "This flight was deleted from this Logbook. Its file is still where it was.",
                ));
            }
            self.regroup_operations();
        }
        for (recording, packed) in file.recordings {
            if self.state.video_recordings.contains_key(&recording.id) {
                continue;
            }
            let state = if recording.state == RecordingState::Recording {
                RecordingState::Done
            } else {
                recording.state
            };
            self.commit_as(
                actor(),
                Change::VideoRecordingUpserted {
                    recording: VideoRecording {
                        packed: Some(packed),
                        state,
                        version: 1,
                        ..recording
                    },
                },
            );
        }
        Ok(id)
    }

    /// The `.uxflight` files Log folders found that the Logbook lacks are
    /// added to it, each tried once. The look that found them says which
    /// it could not read. One whose flight was deleted here stays out.
    pub(crate) fn adopt_found_flight_files(&mut self, paths: &[String]) {
        for path in paths {
            let known = self
                .state
                .flights
                .values()
                .any(|f| f.copies.iter().any(|c| &c.path == path));
            if known || self.offloads.unreadable.contains(path) {
                continue;
            }
            if self.adopt_flight_file(path).is_err() {
                self.offloads.unreadable.insert(path.clone());
            }
        }
    }

    /// A flight's files on the wire: each with whether it is there now, and
    /// its Move or copy under way or failed.
    pub(crate) fn offload_dto(&self, dto: &mut uxduck_contracts::dto::flight::Flight) {
        for copy in &mut dto.copies {
            copy.present = self.system.file_present(&copy.path);
        }
        dto.offload = self.offloads.flights.get(&dto.id).cloned();
    }
}
