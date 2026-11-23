//! The recording's disk: where `MediaMTX` writes a path's segments, the free
//! space there, and room made for the current recording by deleting the
//! oldest finished segments of earlier ones (ADR-0066).
//!
//! There is no fixed limit on how much is kept. Earlier recordings go only
//! when the disk would otherwise not hold the segment being written, and
//! the current recording's segments never go.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use uxduck_mavlink::onboard_camera::Space;

/// Where one path records, and the room its segment being written needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    /// The fixed part of `recordPath`, this path's own folder.
    pub dir: PathBuf,
    /// The segments' file extension, from `recordFormat`.
    pub extension: &'static str,
    /// One segment at the configured bitrate (`recordSegmentDuration` times
    /// `rpiCameraBitrate`), in bytes: the room the segment being written needs
    /// to finish. 0 when the source's bitrate is not configured, and then the
    /// largest segment already written stands in for it.
    pub reserve: u64,
}

/// A finished or growing segment.
#[derive(Clone, Debug)]
pub struct Segment {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

/// What making room did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Made {
    /// Bytes deleted, and in how many files.
    pub deleted: u64,
    pub files: usize,
    /// The space after.
    pub space: Space,
    /// Still less room than the segment being written needs, with no
    /// earlier recording left to delete.
    pub short: bool,
}

impl Store {
    /// The store of `path` from its `MediaMTX` configuration.
    ///
    /// # Errors
    /// A `recordPath` that is not an absolute folder of this path's own.
    pub fn of(path: &str, conf: &serde_json::Value) -> Result<Self, String> {
        let text = |key: &str| conf.get(key).and_then(serde_json::Value::as_str);
        let pattern = text("recordPath").ok_or_else(|| {
            format!("MediaMTX gives {path} no recordPath, so UXDuck cannot tell where it records")
        })?;
        let own = || {
            format!(
                "{path}'s recordPath {pattern} must start with an absolute folder that has %path in it, such as /var/lib/uxduck/recordings/%path/%Y-%m-%d_%H-%M-%S-%f, so UXDuck deletes only this path's recordings"
            )
        };
        let full = Path::new(pattern);
        if !full.is_absolute() {
            return Err(own());
        }
        let mut dir = PathBuf::new();
        let mut named = false;
        for component in full.components() {
            let Component::Normal(part) = component else {
                if matches!(component, Component::ParentDir) {
                    return Err(own());
                }
                dir.push(component);
                continue;
            };
            let part = part.to_string_lossy();
            let filled = part.replace("%path", path);
            if filled.contains('%') {
                break;
            }
            named |= part.contains("%path");
            dir.push(filled);
        }
        if !named || dir == full {
            return Err(own());
        }
        let extension = match text("recordFormat").unwrap_or("fmp4") {
            "fmp4" => "mp4",
            "mpegts" => "ts",
            other => {
                return Err(format!(
                    "{path}'s recordFormat {other} is not fmp4 or mpegts"
                ));
            }
        };
        let segment = text("recordSegmentDuration").unwrap_or("1h");
        let segment = go_duration(segment).ok_or_else(|| {
            format!("{path}'s recordSegmentDuration {segment} is not a duration, such as 5m")
        })?;
        let bitrate = if text("source") == Some("rpiCamera") {
            conf.get("rpiCameraBitrate")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        } else {
            0
        };
        let reserve =
            u64::try_from(u128::from(bitrate / 8) * segment.as_millis() / 1000).unwrap_or(u64::MAX);
        Ok(Self {
            dir,
            extension,
            reserve,
        })
    }

    /// Every segment, oldest first. None while the folder is not there yet.
    ///
    /// # Errors
    /// A folder that cannot be read.
    pub fn segments(&self) -> io::Result<Vec<Segment>> {
        let mut found = Vec::new();
        let mut folders = vec![self.dir.clone()];
        while let Some(folder) = folders.pop() {
            let entries = match std::fs::read_dir(&folder) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for entry in entries {
                let entry = entry?;
                // Never followed: a link could lead out of the folder.
                let meta = entry.path().symlink_metadata()?;
                if meta.is_dir() {
                    folders.push(entry.path());
                } else if meta.is_file()
                    && entry.path().extension().and_then(|e| e.to_str()) == Some(self.extension)
                {
                    found.push(Segment {
                        path: entry.path(),
                        size: meta.len(),
                        modified: meta.modified()?,
                    });
                }
            }
        }
        found.sort_by(|a, b| (a.modified, &a.path).cmp(&(b.modified, &b.path)));
        Ok(found)
    }

    /// The segments there are now, as the ones a recording starting now
    /// may delete.
    ///
    /// # Errors
    /// A folder that cannot be read.
    pub fn earlier(&self) -> io::Result<HashSet<PathBuf>> {
        Ok(self.segments()?.into_iter().map(|s| s.path).collect())
    }

    /// The room the segment being written needs: one at the configured
    /// bitrate, or the largest written when that is larger.
    fn needs(&self, segments: &[Segment]) -> u64 {
        segments
            .iter()
            .map(|s| s.size)
            .max()
            .unwrap_or(0)
            .max(self.reserve)
    }

    /// Deletes the oldest of `earlier` until the segment being written has
    /// room, reading the space with `space` after each.
    ///
    /// # Errors
    /// A folder that cannot be read, a segment that cannot be deleted, or
    /// space that cannot be read.
    pub fn make_room(
        &self,
        earlier: &HashSet<PathBuf>,
        space: impl Fn(&Path) -> io::Result<Space>,
    ) -> io::Result<Made> {
        let segments = self.segments()?;
        let needs = self.needs(&segments);
        let mut made = Made {
            space: space(&self.dir)?,
            ..Made::default()
        };
        for segment in segments.iter().filter(|s| earlier.contains(&s.path)) {
            if made.space.free >= needs {
                break;
            }
            match std::fs::remove_file(&segment.path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
            made.deleted += segment.size;
            made.files += 1;
            made.space = space(&self.dir)?;
        }
        made.short = made.space.free < needs;
        Ok(made)
    }

    /// Whether a recording has room, counting what it may delete: every
    /// segment before it starts, `earlier` once it has.
    ///
    /// # Errors
    /// A folder that cannot be read.
    pub fn room(&self, space: Space, earlier: Option<&HashSet<PathBuf>>) -> io::Result<bool> {
        let segments = self.segments()?;
        let deletable: u64 = segments
            .iter()
            .filter(|s| earlier.is_none_or(|earlier| earlier.contains(&s.path)))
            .map(|s| s.size)
            .sum();
        Ok(space.free.saturating_add(deletable) >= self.needs(&segments))
    }
}

/// The space on the filesystem that holds `dir`, or that will hold it when
/// `MediaMTX` makes it: what an ordinary user may still write.
///
/// # Errors
/// No folder above it that can be read.
pub fn space(dir: &Path) -> io::Result<Space> {
    let there = dir
        .ancestors()
        .find(|folder| folder.exists())
        .unwrap_or(dir);
    filesystem(there)
}

#[cfg(unix)]
fn filesystem(dir: &Path) -> io::Result<Space> {
    let stat = rustix::fs::statvfs(dir)?;
    Ok(Space {
        total: stat.f_blocks.saturating_mul(stat.f_frsize),
        free: stat.f_bavail.saturating_mul(stat.f_frsize),
    })
}

#[cfg(not(unix))]
fn filesystem(_: &Path) -> io::Result<Space> {
    Err(io::Error::other(
        "recording on the vehicle reads free space on Linux only",
    ))
}

/// Bytes as a person reads them, to the nearest MB below a gigabyte and GB
/// with one decimal above, so a size is never overstated.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "a size to one decimal place")]
pub fn size(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    if bytes < GB {
        format!("{} MB", (bytes + 500_000) / 1_000_000)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

/// A Go duration as `MediaMTX` writes one, such as `5m` or `1h30m0s`.
#[must_use]
pub fn go_duration(text: &str) -> Option<Duration> {
    let mut rest = text.trim();
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    let mut total = Duration::ZERO;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|at| *at > 0)?;
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_end] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        rest = &rest[unit_end..];
        total += Duration::try_from_secs_f64(number * seconds).ok()?;
    }
    Some(total)
}
