//! Small previews of photos for a list of them (2026-11-21): a JPEG at most
//! [`LONG_EDGE`] px on its long edge, made from the photo the first time it
//! is asked for and kept in the app folder's `cache/thumbnails`, never in
//! the media folder beside the photo. A preview is named by its photo's id
//! and the photo file's size and time of change, so a photo changed in
//! place gets a new preview and the old one goes, and one whose photo has
//! left the list goes with it ([`sweep`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageError, ImageReader, Limits};

/// The long edge of a preview, in pixels: twice a list's tile on a dense
/// screen.
pub const LONG_EDGE: u32 = 320;

/// The preview's JPEG quality.
const QUALITY: u8 = 80;

const MB: u64 = 1024 * 1024;

/// The most memory decoding one photo for its preview may take. The media
/// folder is the person's to change, so the file behind a photo can be
/// replaced by any picture at all, and a picture is decoded whole: a larger
/// one could run the computer out of memory. UXDuck's own photos are camera
/// frames, about 100 MB decoded at 8K.
pub const MOST_BYTES: u64 = 256 * MB;

/// The widest or tallest picture decoded for a preview, for the same
/// reason: a file that says it is larger is refused before it is decoded.
pub const MOST_EDGE: u32 = 16_384;

/// Previews made at once. Each may hold [`MOST_BYTES`] while it decodes, so
/// a list asking for hundreds together holds two photos' worth at most.
pub const AT_ONCE: usize = 2;

const NOT_A_PICTURE: &str = "UXDuck can't preview this file. It isn't a JPEG, PNG or WebP.";

/// Why a preview could not be made.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The photo's file is not in the media folder.
    Gone,
    /// It can't be made, in words a person reads under the photo's tile.
    Unreadable(String),
}

/// The previews beside the media folder: `<app folder>/cache/thumbnails`.
#[must_use]
pub fn cache_dir(media: &Path) -> PathBuf {
    media
        .parent()
        .unwrap_or(media)
        .join("cache")
        .join("thumbnails")
}

/// A photo id as the engine makes them: letters and digits only, so it is
/// safe in a file's name.
fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A file's own name, never a path out of the media folder.
fn plain_name(name: &str) -> bool {
    Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name)
}

/// The preview of photo `id`, its file `file_name` in `media`: the one kept
/// in `cache` while the photo is unchanged, else made now and kept.
///
/// # Errors
/// [`Failure::Gone`] when the photo's file is not there, and why not in
/// words when it can't be read or decoded within the bounds above.
pub fn get(media: &Path, cache: &Path, id: &str, file_name: &str) -> Result<PathBuf, Failure> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if !plain_id(id) || !plain_name(file_name) {
        return Err(Failure::Unreadable(
            "UXDuck won't preview a file outside its media folder.".into(),
        ));
    }
    let source = media.join(file_name);
    let meta = match std::fs::metadata(&source) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Err(Failure::Gone),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Failure::Gone),
        Err(e) => {
            return Err(Failure::Unreadable(format!(
                "UXDuck can't open the photo: {e}"
            )));
        }
    };
    let changed = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let stem = format!("{id}-{}-{changed}", meta.len());
    let kept = cache.join(format!("{stem}.jpg"));
    if kept.is_file() {
        return Ok(kept);
    }
    let jpeg = make(&source).map_err(Failure::Unreadable)?;
    let cant_keep = |e: std::io::Error| {
        tracing::warn!(cache = %cache.display(), %e, "couldn't keep a photo's preview");
        Failure::Unreadable(format!("UXDuck couldn't keep the photo's preview: {e}"))
    };
    std::fs::create_dir_all(cache).map_err(cant_keep)?;
    // Written whole under another name first, so a preview asked for twice
    // at once is never read half written.
    let part = cache.join(format!(
        "{stem}.{}-{}.part",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&part, &jpeg)
        .and_then(|()| std::fs::rename(&part, &kept))
        .map_err(|e| {
            let _ = std::fs::remove_file(&part);
            cant_keep(e)
        })?;
    // The previews of this photo before it changed.
    let mine = format!("{id}-");
    let now = format!("{stem}.jpg");
    if let Ok(entries) = std::fs::read_dir(cache) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&mine) && has_extension(name, "jpg") && name != now {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(kept)
}

fn has_extension(name: &str, extension: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(extension))
}

/// Why the picture couldn't be decoded, in words. The decoder's own words
/// go to the log.
fn unreadable(path: &Path, error: &ImageError) -> String {
    tracing::warn!(photo = %path.display(), %error, "couldn't decode a photo for its preview");
    match error {
        ImageError::Limits(_) => format!(
            "The photo is too large to preview. UXDuck decodes at most {MOST_EDGE} × {MOST_EDGE} \
             pixels in {} MB for a preview.",
            MOST_BYTES / MB
        ),
        ImageError::Unsupported(_) => NOT_A_PICTURE.into(),
        _ => "UXDuck can't read the photo. The file may be damaged.".into(),
    }
}

/// A JPEG of the picture at `path`, upright and no longer than
/// [`LONG_EDGE`] on its long side, never larger than the picture.
///
/// # Errors
/// Why not, in words: it can't be opened, isn't a picture UXDuck decodes,
/// is damaged, or is larger than [`MOST_EDGE`] or [`MOST_BYTES`] allow.
pub fn make(path: &Path) -> Result<Vec<u8>, String> {
    // Known by what it holds, never by its name: a file renamed .jpg may
    // be anything.
    let mut reader = std::fs::File::open(path)
        .map(|file| ImageReader::new(std::io::BufReader::new(file)))
        .and_then(ImageReader::with_guessed_format)
        .map_err(|e| format!("UXDuck can't open the photo: {e}"))?;
    if reader.format().is_none() {
        return Err(NOT_A_PICTURE.into());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MOST_EDGE);
    limits.max_image_height = Some(MOST_EDGE);
    limits.max_alloc = Some(MOST_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| unreadable(path, &e))?;
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let picture = DynamicImage::from_decoder(decoder).map_err(|e| unreadable(path, &e))?;
    let mut small = if width.max(height) > LONG_EDGE {
        picture.thumbnail(LONG_EDGE, LONG_EDGE)
    } else {
        picture
    };
    small.apply_orientation(orientation);
    let mut jpeg = Vec::new();
    DynamicImage::ImageRgb8(small.to_rgb8())
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut jpeg, QUALITY,
        ))
        .map_err(|e| {
            tracing::warn!(photo = %path.display(), %e, "couldn't write a photo's preview");
            "UXDuck couldn't make the photo's preview.".to_owned()
        })?;
    Ok(jpeg)
}

/// Removes the previews of photos no longer in the list, and any left half
/// written: `photos` is the ids of every photo UXDuck has now. How many
/// went.
pub fn sweep(cache: &Path, photos: &HashSet<String>) -> usize {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let id = name.split('-').next().unwrap_or_default();
        let stale = has_extension(name, "part") || !photos.contains(id);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(dir: &Path, name: &str, width: u32, height: u32) {
        let picture = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                u8::try_from(x % 256).unwrap(),
                u8::try_from(y % 256).unwrap(),
                128,
            ])
        });
        let format = if has_extension(name, "png") {
            image::ImageFormat::Png
        } else {
            image::ImageFormat::Jpeg
        };
        picture.save_with_format(dir.join(name), format).unwrap();
    }

    fn size(jpeg: &Path) -> (u32, u32) {
        image::image_dimensions(jpeg).unwrap()
    }

    fn media() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let app = tempfile::tempdir().unwrap();
        let media = app.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let cache = cache_dir(&media);
        (app, media, cache)
    }

    #[test]
    fn a_preview_is_made_small_and_kept_in_the_cache() {
        let (app, media, cache) = media();
        photo(&media, "a.jpg", 1280, 720);
        assert_eq!(cache, app.path().join("cache").join("thumbnails"));
        let made = get(&media, &cache, "vp1", "a.jpg").unwrap();
        assert!(made.starts_with(&cache), "{}", made.display());
        assert_eq!(size(&made), (320, 180));
        // Nothing beside the photo.
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 1);
        // Asked again, the kept one is given, not made again.
        let at = std::fs::metadata(&made).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let again = get(&media, &cache, "vp1", "a.jpg").unwrap();
        assert_eq!(again, made);
        assert_eq!(std::fs::metadata(&again).unwrap().modified().unwrap(), at);
    }

    #[test]
    fn a_small_photo_is_not_made_larger() {
        let (_app, media, cache) = media();
        photo(&media, "small.png", 200, 100);
        let made = get(&media, &cache, "vp2", "small.png").unwrap();
        assert_eq!(size(&made), (200, 100));
    }

    #[test]
    fn a_photo_changed_in_place_gets_a_new_preview_and_the_old_goes() {
        let (_app, media, cache) = media();
        photo(&media, "a.jpg", 640, 480);
        let first = get(&media, &cache, "vp1", "a.jpg").unwrap();
        assert_eq!(size(&first), (320, 240));
        photo(&media, "a.jpg", 480, 640);
        let second = get(&media, &cache, "vp1", "a.jpg").unwrap();
        assert_ne!(first, second);
        assert_eq!(size(&second), (240, 320));
        assert!(!first.exists());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[test]
    fn previews_of_photos_gone_from_the_list_are_swept() {
        let (app, media, cache) = media();
        photo(&media, "a.jpg", 400, 300);
        photo(&media, "b.jpg", 400, 300);
        let a = get(&media, &cache, "vpa", "a.jpg").unwrap();
        let b = get(&media, &cache, "vpb", "b.jpg").unwrap();
        std::fs::write(cache.join("vpa-1-2.123-0.part"), b"half").unwrap();
        let kept: HashSet<String> = ["vpa".to_owned()].into();
        assert_eq!(sweep(&cache, &kept), 2);
        assert!(a.exists());
        assert!(!b.exists());
        assert_eq!(sweep(&app.path().join("nowhere"), &kept), 0);
    }

    #[test]
    fn a_deleted_photo_is_gone() {
        let (_app, media, cache) = media();
        assert_eq!(get(&media, &cache, "vp1", "a.jpg"), Err(Failure::Gone));
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_refused_in_words() {
        let (_app, media, cache) = media();
        std::fs::write(media.join("a.jpg"), b"not a picture at all").unwrap();
        let Err(Failure::Unreadable(why)) = get(&media, &cache, "vp1", "a.jpg") else {
            panic!("made a preview of nothing");
        };
        assert!(why.contains("isn't a JPEG, PNG or WebP"), "{why}");
        // A JPEG cut short says it may be damaged.
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::new(64, 64))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        std::fs::write(media.join("b.jpg"), &jpeg[..jpeg.len() / 3]).unwrap();
        let Err(Failure::Unreadable(why)) = get(&media, &cache, "vp2", "b.jpg") else {
            panic!("made a preview of a third of a JPEG");
        };
        assert!(why.contains("damaged"), "{why}");
        // Neither left a preview behind.
        assert!(std::fs::read_dir(&cache).map_or(true, |mut d| d.next().is_none()));
    }

    #[test]
    fn a_picture_larger_than_the_bound_is_refused_before_it_is_decoded() {
        let (_app, media, cache) = media();
        // A PNG whose header says 100 000 × 100 000 pixels, 30 GB decoded,
        // and nothing after it.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        std::fs::write(media.join("huge.jpg"), &png).unwrap();
        let Err(Failure::Unreadable(why)) = get(&media, &cache, "vp1", "huge.jpg") else {
            panic!("decoded a 30 GB picture");
        };
        assert!(why.contains("too large to preview"), "{why}");
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn names_that_reach_outside_the_folder_are_refused() {
        let (app, media, cache) = media();
        photo(app.path(), "outside.jpg", 64, 64);
        for (id, name) in [
            ("vp1", "../outside.jpg"),
            ("vp1", "/etc/passwd"),
            ("vp1", ".."),
            ("vp1", ""),
            ("../vp1", "a.jpg"),
            ("vp-1", "a.jpg"),
            ("", "a.jpg"),
        ] {
            assert!(
                matches!(get(&media, &cache, id, name), Err(Failure::Unreadable(_))),
                "{id} {name}"
            );
        }
    }
}
