//! The pictures the AI model is shown of those attached to a message
//! (FB-248): a PNG, JPEG or WebP decoded, turned upright as its camera
//! says, scaled to fit the model's long edge and room, and written again
//! as a JPEG, off the engine's thread and one at a time.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use image::metadata::Orientation;
use image::{
    ColorType, DynamicImage, ImageDecoder as _, ImageError, ImageReader, Limits, RgbImage,
};
use tokio::sync::mpsc;
use uxduck_engine::runtime::{Picture, PictureOrder};

use crate::daemon::engine_task::EngineHandle;

const MB: u64 = 1024 * 1024;

/// The most memory UXDuck gives to making the model's copy of one picture:
/// the larger of decoding it (the picture as its file holds it, and the
/// 8-bit copy made of it when it is another kind) and scaling it (the 8-bit
/// copy, the scaler's working copy of 16 bytes a pixel at the picture's
/// width and the scaled height, and the scaled copy). A picture is decoded
/// whole, so more could run the computer out of memory. Pictures are made
/// one at a time, so this and the decoder's own working room are the most
/// held at once.
const MOST_BYTES: u64 = 400 * MB;

/// The decoder's own working room beside the picture it decodes. The
/// decoder is held to `MOST_BYTES` and this together.
const DECODER_ROOM: u64 = 64 * MB;

/// Bytes a pixel of the scaler's working copy, which `imageops::resize`
/// keeps as four 32-bit floats.
const SCALER_BYTES: u64 = 16;

/// The smallest long edge worth showing; smaller than this a picture says
/// too little to trace.
const SMALLEST_EDGE: u32 = 160;

/// The JPEG's quality, as camera frames are written.
const QUALITY: u8 = 85;

/// Makes each picture asked for, one at a time off the engine's thread so
/// only one is held in memory, and gives the engine what came of it.
pub async fn work(
    mut orders: mpsc::UnboundedReceiver<(PictureOrder, PathBuf)>,
    engine: EngineHandle,
) {
    while let Some((order, path)) = orders.recv().await {
        let key = order.key.clone();
        let made =
            tokio::task::spawn_blocking(move || make(&path, order.long_edge, order.max_bytes))
                .await
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "making an attached picture for the AI model failed");
                    Err(
                        "UXDuck couldn't make a copy of the picture for the AI model. Try again."
                            .to_owned(),
                    )
                });
        engine.post(move |e| e.attachment_pictured(&key, made));
    }
}

/// The name a person knows the file by.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "The picture".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Why the picture at `path` couldn't be decoded, in words for a person.
/// The decoder's own words go to the log.
fn unreadable(path: &Path, error: &ImageError) -> String {
    tracing::warn!(picture = %path.display(), %error, "couldn't decode an attached picture");
    let name = name(path);
    match error {
        ImageError::Limits(_) => format!(
            "{name} needs more memory to decode than UXDuck gives a picture's decoder, {} MB. \
             Attach a smaller copy.",
            (MOST_BYTES + DECODER_ROOM) / MB
        ),
        ImageError::Unsupported(_) => {
            format!("UXDuck can't read {name}. It is not a kind of PNG, JPEG or WebP it knows.")
        }
        _ => format!("UXDuck couldn't read {name} as a picture. The file may be damaged."),
    }
}

/// A JPEG of the picture at `path`, upright, no longer than `long_edge` on
/// its longest side, never larger than the picture, and made smaller by a
/// tenth at a time until it is no more than `max_bytes`.
///
/// # Errors
/// Why not, in words: it can't be read or decoded, it needs more memory to
/// decode than UXDuck gives it, or even its smallest copy doesn't fit
/// `max_bytes`.
pub fn make(path: &Path, long_edge: u32, max_bytes: u64) -> Result<Picture, String> {
    let name = name(path);
    let mut reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|error| {
            tracing::warn!(picture = %path.display(), %error, "couldn't open an attached picture");
            format!("UXDuck couldn't open {name}.")
        })?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MOST_BYTES + DECODER_ROOM);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| unreadable(path, &e))?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    let orientation = decoder.orientation().unwrap_or_else(|error| {
        tracing::warn!(picture = %path.display(), %error, "an attached picture's orientation is unreadable");
        Orientation::NoTransforms
    });
    let turned = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let (source_width, source_height) = if turned {
        (height, width)
    } else {
        (width, height)
    };
    // As the picture is stored, before it is turned upright.
    let stored = |(w, h): (u32, u32)| if turned { (h, w) } else { (w, h) };
    let need = memory(
        (width, height),
        color,
        stored(fit(
            source_width,
            source_height,
            long_edge.min(source_width.max(source_height)),
        )),
    );
    if need > MOST_BYTES {
        return Err(format!(
            "{name} is {width} × {height} pixels at {} bits a pixel, which takes {} MB of memory \
             to decode and scale. UXDuck decodes a picture whole to show it to the AI model, in at \
             most {} MB. Attach a smaller copy.",
            color.bits_per_pixel(),
            need.div_ceil(MB),
            MOST_BYTES / MB
        ));
    }
    let rgb = flatten(DynamicImage::from_decoder(decoder).map_err(|e| unreadable(path, &e))?)
        .ok_or_else(|| format!("UXDuck couldn't make a copy of {name} for the AI model."))?;
    let mut edge = long_edge.min(source_width.max(source_height));
    loop {
        let (out_width, out_height) = fit(source_width, source_height, edge);
        let stored = stored((out_width, out_height));
        // Scaled before it is turned, so the whole picture is never copied.
        let mut scaled = DynamicImage::ImageRgb8(if stored == (width, height) {
            rgb.clone()
        } else {
            image::imageops::resize(
                &rgb,
                stored.0,
                stored.1,
                image::imageops::FilterType::CatmullRom,
            )
        });
        scaled.apply_orientation(orientation);
        let mut jpeg = Vec::new();
        scaled
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut jpeg, QUALITY,
            ))
            .map_err(|error| {
                tracing::warn!(picture = %path.display(), %error, "couldn't write an attached picture as a JPEG");
                format!("UXDuck couldn't make a JPEG of {name} for the AI model.")
            })?;
        if jpeg.len() as u64 <= max_bytes {
            return Ok(Picture {
                source_width,
                source_height,
                width: out_width,
                height: out_height,
                jpeg_base64: base64::engine::general_purpose::STANDARD.encode(&jpeg),
            });
        }
        if edge <= SMALLEST_EDGE {
            return Err(format!(
                "Even at {out_width} × {out_height}, {name} comes to {} KB as a JPEG, more than the \
                 {} KB this chat's AI model has room for in this request. Start a new chat, or ask \
                 about fewer pictures at once.",
                (jpeg.len() as u64).div_ceil(1024),
                max_bytes / 1024
            ));
        }
        edge = (edge - edge / 10).max(SMALLEST_EDGE);
    }
}

/// The most memory, in bytes, making a copy of a `size` picture of `color`
/// takes, scaled first to `scaled` as it is stored: the larger of decoding
/// it and scaling it, as `MOST_BYTES` says.
fn memory(size: (u32, u32), color: ColorType, scaled: (u32, u32)) -> u64 {
    let pixels = u64::from(size.0) * u64::from(size.1);
    let copy = match color {
        ColorType::Rgb8 | ColorType::Rgba8 => 0,
        other if other.has_alpha() => 4,
        _ => 3,
    };
    let decoding = pixels * (u64::from(color.bytes_per_pixel()) + copy);
    // `flatten` keeps an RGBA buffer's room after laying it over white.
    let kept = pixels * if color.has_alpha() { 4 } else { 3 };
    let scaling = if scaled == size {
        kept + 3 * pixels
    } else {
        kept + SCALER_BYTES * u64::from(size.0) * u64::from(scaled.1)
            + 3 * u64::from(scaled.0) * u64::from(scaled.1)
    };
    decoding.max(scaling)
}

/// The size that fits `edge` on the longest side, in proportion.
fn fit(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= edge {
        return (width, height);
    }
    let scale = |side: u32| {
        u32::try_from(u64::from(side) * u64::from(edge) / u64::from(long))
            .unwrap_or(edge)
            .max(1)
    };
    (scale(width), scale(height))
}

/// The picture as 8-bit RGB, anything see-through laid over white, as the
/// map and drawings are seen in the app. It is consumed, and a picture with
/// alpha is laid over white in its own buffer, so no third copy is made.
fn flatten(picture: DynamicImage) -> Option<RgbImage> {
    if !picture.color().has_alpha() {
        return Some(picture.into_rgb8());
    }
    let rgba = picture.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut raw = rgba.into_raw();
    let over = |c: u8, a: u8| {
        u8::try_from((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255)
            .unwrap_or(u8::MAX)
    };
    let pixels = raw.len() / 4;
    for i in 0..pixels {
        let a = raw[4 * i + 3];
        for c in 0..3 {
            raw[3 * i + c] = over(raw[4 * i + c], a);
        }
    }
    raw.truncate(pixels * 3);
    RgbImage::from_raw(width, height, raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::io::Cursor;

    use base64::Engine as _;
    use image::{ImageFormat, Rgba, RgbaImage};

    use super::make;

    /// A PNG chunk, its CRC worked out as PNG's specification does.
    fn chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
        let mut crc = 0xffff_ffff_u32;
        for byte in kind.iter().chain(data) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    0xedb8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&(!crc).to_be_bytes());
        out
    }

    /// A JPEG whose EXIF says its camera was turned a quarter clockwise
    /// (orientation 6), as a phone held upright writes one.
    fn upright_phone_photo(dir: &std::path::Path, width: u32, height: u32) -> std::path::PathBuf {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(width, height, image::Rgb([90, 140, 60]))
            .write_to(&mut bytes, ImageFormat::Jpeg)
            .unwrap();
        let jpeg = bytes.into_inner();
        let mut exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01".to_vec();
        exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
        let mut out = vec![0xff, 0xd8, 0xff, 0xe1];
        out.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&jpeg[2..]);
        let path = dir.join("phone.jpg");
        std::fs::write(&path, out).unwrap();
        path
    }

    fn write(
        dir: &std::path::Path,
        name: &str,
        width: u32,
        height: u32,
        format: ImageFormat,
    ) -> std::path::PathBuf {
        let picture = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                u8::try_from(x % 256).unwrap(),
                u8::try_from(y % 256).unwrap(),
                80,
                if x < 10 { 0 } else { 255 },
            ])
        });
        let path = dir.join(name);
        let mut bytes = Cursor::new(Vec::new());
        if format == ImageFormat::Jpeg {
            image::DynamicImage::ImageRgba8(picture)
                .to_rgb8()
                .write_to(&mut bytes, format)
                .unwrap();
        } else {
            picture.write_to(&mut bytes, format).unwrap();
        }
        std::fs::write(&path, bytes.into_inner()).unwrap();
        path
    }

    fn jpeg_size(picture: &uxduck_engine::runtime::Picture) -> (u32, u32) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&picture.jpeg_base64)
            .unwrap();
        assert_eq!(&bytes[..3], &[0xff, 0xd8, 0xff], "a JPEG");
        let read = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).unwrap();
        (read.width(), read.height())
    }

    #[test]
    fn png_jpeg_and_webp_become_a_jpeg_of_at_most_1024_px_never_enlarged() {
        let dir = tempfile::tempdir().unwrap();
        for (name, format) in [
            ("a.png", ImageFormat::Png),
            ("b.jpg", ImageFormat::Jpeg),
            ("c.webp", ImageFormat::WebP),
        ] {
            let made = make(
                &write(dir.path(), name, 3000, 1500, format),
                1024,
                8 * 1024 * 1024,
            )
            .unwrap();
            assert_eq!(
                (made.source_width, made.source_height),
                (3000, 1500),
                "{name}"
            );
            assert_eq!((made.width, made.height), (1024, 512), "{name}");
            assert_eq!(jpeg_size(&made), (1024, 512), "{name}");
        }
        let small = make(
            &write(dir.path(), "small.png", 300, 200, ImageFormat::Png),
            1024,
            8 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!((small.width, small.height), (300, 200));
        let tall = make(
            &write(dir.path(), "tall.png", 900, 2700, ImageFormat::Png),
            1024,
            8 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!((tall.width, tall.height), (341, 1024));
    }

    #[test]
    fn a_picture_is_made_smaller_to_fit_the_room_and_refused_in_words_when_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "field.png", 2000, 2000, ImageFormat::Png);
        let roomy = make(&path, 1024, 8 * 1024 * 1024).unwrap();
        let room = 20 * 1024;
        let made = make(&path, 1024, room).unwrap();
        assert!(made.width < roomy.width, "{} × {}", made.width, made.height);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&made.jpeg_base64)
            .unwrap();
        assert!(bytes.len() as u64 <= room);
        let refused = make(&path, 1024, 100).unwrap_err();
        assert!(
            refused.contains("field.png") && refused.contains("room for"),
            "{refused}"
        );
        assert!(!refused.contains(';'));
    }

    #[test]
    fn a_damaged_picture_is_refused_in_plain_words_not_the_decoders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(chunk(*b"IDAT", b"before its header"));
        std::fs::write(&path, bytes).unwrap();
        let refused = make(&path, 1024, 8 * 1024 * 1024).unwrap_err();
        assert_eq!(
            refused,
            "UXDuck couldn't read broken.png as a picture. The file may be damaged."
        );
    }

    #[test]
    fn a_picture_that_needs_more_memory_than_uxduck_gives_says_how_much_by_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        // 81 megapixels of 16-bit RGBA: 8 bytes a pixel, then a 4-byte copy.
        let mut header = Vec::new();
        header.extend_from_slice(&9000_u32.to_be_bytes());
        header.extend_from_slice(&9000_u32.to_be_bytes());
        header.extend_from_slice(&[16, 6, 0, 0, 0]);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(chunk(*b"IHDR", &header));
        bytes.extend(chunk(*b"IDAT", &[]));
        bytes.extend(chunk(*b"IEND", &[]));
        let path = dir.path().join("survey.png");
        std::fs::write(&path, bytes).unwrap();
        let refused = make(&path, 1024, 8 * 1024 * 1024).unwrap_err();
        assert_eq!(
            refused,
            "survey.png is 9000 × 9000 pixels at 64 bits a pixel, which takes 927 MB of memory to \
             decode and scale. UXDuck decodes a picture whole to show it to the AI model, in at \
             most 400 MB. Attach a smaller copy."
        );
    }

    #[test]
    fn scaling_a_picture_counts_the_scalers_working_copy() {
        use image::ColorType;
        // 11000 × 11000 RGB decodes in 346 MB, under the 400, but scaling it
        // to 1024 × 1024 holds that, a 16-byte working copy 11000 wide and
        // 1024 high (172 MB) and the scaled copy (3 MB): 522 MB, rounded up.
        assert_eq!(
            super::memory((11_000, 11_000), ColorType::Rgb8, (1024, 1024)),
            363_000_000 + 16 * 11_000 * 1024 + 3 * 1024 * 1024
        );
        // Decoding is the larger part of a 16-bit picture.
        assert_eq!(
            super::memory((9000, 9000), ColorType::Rgba16, (1024, 1024)),
            81_000_000 * 12
        );
        // A picture small enough to keep its size is copied once.
        assert_eq!(
            super::memory((800, 600), ColorType::Rgba8, (800, 600)),
            480_000 * 7
        );

        let dir = tempfile::tempdir().unwrap();
        let mut header = Vec::new();
        header.extend_from_slice(&11_000_u32.to_be_bytes());
        header.extend_from_slice(&11_000_u32.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(chunk(*b"IHDR", &header));
        bytes.extend(chunk(*b"IDAT", &[]));
        bytes.extend(chunk(*b"IEND", &[]));
        let path = dir.path().join("orthophoto.png");
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(
            make(&path, 1024, 8 * 1024 * 1024).unwrap_err(),
            "orthophoto.png is 11000 × 11000 pixels at 24 bits a pixel, which takes 522 MB of \
             memory to decode and scale. UXDuck decodes a picture whole to show it to the AI model, \
             in at most 400 MB. Attach a smaller copy."
        );
    }

    #[test]
    fn a_phone_photo_is_turned_upright_as_its_exif_says() {
        let dir = tempfile::tempdir().unwrap();
        let small = make(
            &upright_phone_photo(dir.path(), 300, 100),
            1024,
            8 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!((small.source_width, small.source_height), (100, 300));
        assert_eq!((small.width, small.height), (100, 300));
        assert_eq!(jpeg_size(&small), (100, 300));
        let large = make(
            &upright_phone_photo(dir.path(), 3000, 1500),
            1024,
            8 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!((large.source_width, large.source_height), (1500, 3000));
        assert_eq!((large.width, large.height), (512, 1024));
        assert_eq!(jpeg_size(&large), (512, 1024));
    }
}
