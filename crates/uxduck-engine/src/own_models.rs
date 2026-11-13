//! A vehicle's own 3D model (ADR-0035 M5, 2026-11-12): a glTF file the
//! person gave, checked here, kept by the host under its SHA-256 and read
//! back by the pages that draw it. Both renderers draw it, the 3D map and
//! the vehicle view, so a file either cannot draw is refused when it is
//! given, in words, rather than failing later on a map. One both can draw
//! but more detailed than a vehicle on a map needs is kept with a warning
//! in words, because detail nobody sees still slows the maps down.

use std::collections::HashMap;
use std::fmt::Write as _;

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::vehicle::OWN_MODEL_MAX;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::vehicle::OwnModel;

use crate::{ConnectionId, Engine};

/// The glTF extensions a file may require: those both renderers draw with
/// no decoder of their own. Draco and meshopt compression and KTX2
/// textures need one.
const DRAWN_EXTENSIONS: &[&str] = &[
    "KHR_mesh_quantization",
    "KHR_texture_transform",
    "KHR_materials_unlit",
    "EXT_texture_webp",
];

/// The turns a model can be given, clockwise from above.
const TURNS: [u16; 4] = [0, 90, 180, 270];

/// The widest texture a model may have: some graphics cards hold none wider.
const TEXTURE_MAX_PX: u32 = 8192;

/// The most a model's textures may take on the graphics card, which every
/// view drawing it holds a copy of: past this a computer with integrated
/// graphics can stop drawing the maps. A 4096 px texture takes 85 MB.
const TEXTURE_MAX_BYTES: u64 = 256 * 1_000_000;

/// Past these a model is kept with a warning, as it slows the maps down
/// rather than stopping them. UXDuck's own have 600 to 8,000 triangles, 1
/// to 21 parts and no textures, and a vehicle on a map is a few dozen
/// pixels across. 64 MB is three 2048 px textures.
const TRIANGLES_BUDGET: u64 = 100_000;
const TEXTURE_BUDGET_BYTES: u64 = 64 * 1_000_000;
const PARTS_BUDGET: u64 = 100;

/// The texture size the advice gives.
const TEXTURE_ADVICE_PX: u32 = 2048;

/// What drawing a model costs, whatever its file's size: the triangles and
/// separately drawn parts (a mesh's primitives, as often as nodes place
/// it) of its scene, its widest texture, and what its textures take on
/// the graphics card, mipmaps included.
#[derive(Debug, Default, PartialEq, Eq)]
struct Detail {
    triangles: u64,
    parts: u64,
    texture_px: u32,
    texture_bytes: u64,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// "18.4 MB".
fn megabytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a size in words")]
    let mb = bytes as f64 / 1e6;
    format!("{mb:.1} MB")
}

/// A glTF's JSON: a binary glTF's first chunk, or the whole of a `.gltf`.
/// With a binary glTF, its binary chunk too, empty when it has none.
fn json_of<'a>(name: &str, bytes: &'a [u8]) -> Result<(Value, Option<&'a [u8]>), ApiError> {
    let not_gltf = || {
        invalid(format!(
            "{name} is not a glTF model. Choose a .glb or .gltf file."
        ))
    };
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
    };
    if bytes.starts_with(b"glTF") {
        let version = word(4).ok_or_else(not_gltf)?;
        if version != 2 {
            return Err(invalid(format!(
                "{name} is glTF {version}. UXDuck draws glTF 2.0 models."
            )));
        }
        if word(8).map(u64::from) != Some(bytes.len() as u64) {
            return Err(invalid(format!("{name} is cut short. Export it again.")));
        }
        // The first chunk is the JSON, as the format requires.
        let length = word(12).ok_or_else(not_gltf)? as usize;
        if word(16) != Some(0x4E4F_534A) {
            return Err(not_gltf());
        }
        let json = bytes.get(20..20 + length).ok_or_else(not_gltf)?;
        let bin: &[u8] = match (word(20 + length), word(24 + length)) {
            (Some(size), Some(0x004E_4942)) => bytes
                .get(28 + length..28 + length + size as usize)
                .ok_or_else(not_gltf)?,
            _ => &[],
        };
        return Ok((
            serde_json::from_slice(json).map_err(|_| not_gltf())?,
            Some(bin),
        ));
    }
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if text.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(not_gltf());
    }
    Ok((serde_json::from_slice(text).map_err(|_| not_gltf())?, None))
}

/// A `data:` URI's bytes, if it is base64.
fn data_uri(uri: &str) -> Option<Vec<u8>> {
    let (head, data) = uri.strip_prefix("data:")?.split_once(',')?;
    if !head.ends_with(";base64") {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// A PNG's, JPEG's or WebP's width and height, from its first bytes.
fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| {
        Some(u32::from(u16::from_be_bytes(
            bytes.get(at..at + 2)?.try_into().ok()?,
        )))
    };
    let le16 = |at: usize| {
        Some(u32::from(u16::from_le_bytes(
            bytes.get(at..at + 2)?.try_into().ok()?,
        )))
    };
    let le24 = |at: usize| {
        let b = bytes.get(at..at + 3)?;
        Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        return Some((be32(16)?, be32(20)?));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut at = 2;
        while *bytes.get(at)? == 0xFF {
            let marker = *bytes.get(at + 1)?;
            match marker {
                0xFF => at += 1,
                0xD8 | 0x01 | 0xD0..=0xD7 => at += 2,
                0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                    return Some((be16(at + 7)?, be16(at + 5)?));
                }
                0xDA | 0xD9 => return None,
                _ => at += 2 + be16(at + 2)? as usize,
            }
        }
        return None;
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return match bytes.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            _ => None,
        };
    }
    None
}

/// What drawing this glTF costs, from its JSON and the first bytes of its
/// images, without drawing it.
fn detail_of(json: &Value, bin: Option<&[u8]>) -> Detail {
    let list = |key: &str| json[key].as_array().map_or(&[][..], Vec::as_slice);
    let count = |accessor: &Value| {
        accessor
            .as_u64()
            .and_then(|i| list("accessors").get(usize::try_from(i).ok()?))
            .and_then(|a| a["count"].as_u64())
            .unwrap_or(0)
    };
    let meshes: Vec<(u64, u64)> = list("meshes")
        .iter()
        .map(|mesh| {
            let primitives = mesh["primitives"].as_array().map_or(&[][..], Vec::as_slice);
            let triangles = primitives
                .iter()
                .map(|p| {
                    let n = if p["indices"].is_null() {
                        count(&p["attributes"]["POSITION"])
                    } else {
                        count(&p["indices"])
                    };
                    match p["mode"].as_u64().unwrap_or(4) {
                        4 => n / 3,
                        5 | 6 => n.saturating_sub(2),
                        _ => 0,
                    }
                })
                .sum();
            (triangles, primitives.len() as u64)
        })
        .collect();
    let nodes = list("nodes");
    let index = |v: &Value| v.as_u64().and_then(|i| usize::try_from(i).ok());
    let mut detail = Detail::default();
    match list("scenes").get(index(&json["scene"]).unwrap_or(0)) {
        Some(scene) => {
            let mut stack: Vec<usize> = scene["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(index)
                .collect();
            // A node is in a scene once. A file that has one in it twice
            // or in a loop is counted no further than every node once.
            let mut visits = 0;
            while let Some(at) = stack.pop() {
                visits += 1;
                if visits > nodes.len() {
                    break;
                }
                let Some(node) = nodes.get(at) else { continue };
                if let Some((triangles, parts)) = index(&node["mesh"]).and_then(|m| meshes.get(m)) {
                    detail.triangles += triangles;
                    detail.parts += parts;
                }
                stack.extend(
                    node["children"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(index),
                );
            }
        }
        None => {
            for (triangles, parts) in &meshes {
                detail.triangles += triangles;
                detail.parts += parts;
            }
        }
    }
    for (width, height) in image_sizes(json, bin) {
        detail.texture_px = detail.texture_px.max(width).max(height);
        detail.texture_bytes += u64::from(width) * u64::from(height) * 4 * 4 / 3;
    }
    detail
}

/// Each image's width and height, from its first bytes, where it says.
fn image_sizes(json: &Value, bin: Option<&[u8]>) -> Vec<(u32, u32)> {
    let list = |key: &str| json[key].as_array().map_or(&[][..], Vec::as_slice);
    let index = |v: &Value| v.as_u64().and_then(|i| usize::try_from(i).ok());
    let mut buffers: HashMap<usize, Option<Vec<u8>>> = HashMap::new();
    let mut sizes = Vec::new();
    for image in list("images") {
        let size = if let Some(uri) = image["uri"].as_str() {
            data_uri(uri).as_deref().and_then(image_size)
        } else {
            let Some(view) = index(&image["bufferView"]).and_then(|v| list("bufferViews").get(v))
            else {
                continue;
            };
            let Some(buffer) = index(&view["buffer"]) else {
                continue;
            };
            let data = buffers.entry(buffer).or_insert_with(|| {
                let uri = list("buffers")
                    .get(buffer)?
                    .get("uri")
                    .and_then(Value::as_str);
                match uri {
                    Some(uri) => data_uri(uri),
                    None if buffer == 0 => bin.map(<[u8]>::to_vec),
                    None => None,
                }
            });
            let start = index(&view["byteOffset"]).unwrap_or(0);
            let end = start.saturating_add(index(&view["byteLength"]).unwrap_or(0));
            data.as_deref()
                .and_then(|d| d.get(start..end))
                .and_then(image_size)
        };
        sizes.extend(size);
    }
    sizes
}

/// "340 MB".
fn whole_megabytes(bytes: u64) -> String {
    format!("{} MB", many(bytes.div_ceil(1_000_000)))
}

/// "1.2 million" or "250,000".
fn many(n: u64) -> String {
    if n >= 1_000_000 {
        #[allow(clippy::cast_precision_loss, reason = "a count in words")]
        let millions = n as f64 / 1e6;
        return format!("{millions:.1} million");
    }
    let digits = n.to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// Why a model would slow the maps down and what to do about it, in words,
/// when it is more detailed than a vehicle needs.
fn warning(name: &str, detail: &Detail) -> Option<String> {
    let mut has = Vec::new();
    let mut fixes = Vec::new();
    if detail.triangles > TRIANGLES_BUDGET {
        has.push(format!("{} triangles", many(detail.triangles)));
        fixes.push("reduce its triangles with Blender's Decimate modifier".to_owned());
    }
    if detail.texture_bytes > TEXTURE_BUDGET_BYTES {
        has.push(format!(
            "textures that take {} on the graphics card",
            whole_megabytes(detail.texture_bytes)
        ));
        fixes.push(format!(
            "export fewer textures, at {TEXTURE_ADVICE_PX} px at most"
        ));
    }
    if detail.parts > PARTS_BUDGET {
        has.push(format!("{} separately drawn parts", many(detail.parts)));
        fixes.push("join its meshes and share materials".to_owned());
    }
    let and = |items: &[String]| match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    if has.is_empty() {
        return None;
    }
    let mut fix = and(&fixes);
    if let Some(first) = fix.get(..1) {
        fix = first.to_uppercase() + &fix[1..];
    }
    Some(format!(
        "{name} has {}, more than a vehicle on a map needs, so the maps may slow down. {fix}.",
        and(&has)
    ))
}

/// Whether both renderers can draw this file, in words when they cannot,
/// and a warning when it is more detailed than a vehicle needs.
///
/// # Errors
/// Why not: too big, not glTF 2.0, cut short, with parts in other files,
/// nothing to draw, an extension neither renderer draws, or a texture
/// wider than a graphics card may hold.
pub(crate) fn check(name: &str, bytes: &[u8]) -> Result<Option<String>, ApiError> {
    if bytes.len() as u64 > OWN_MODEL_MAX {
        return Err(invalid(format!(
            "{name} is {}. A model of your own is at most 32 MB.",
            megabytes(bytes.len() as u64)
        )));
    }
    let (json, bin) = json_of(name, bytes)?;
    let version = json["asset"]["version"].as_str().unwrap_or_default();
    if !version.starts_with("2.") {
        return Err(invalid(format!(
            "{name} is glTF {}. UXDuck draws glTF 2.0 models.",
            if version.is_empty() {
                "of no version"
            } else {
                version
            }
        )));
    }
    // A binary glTF's own buffer has no uri. Anything else must be inside.
    let elsewhere = ["buffers", "images"].iter().any(|list| {
        json[*list].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["uri"]
                    .as_str()
                    .is_some_and(|uri| !uri.starts_with("data:"))
            })
        })
    });
    if elsewhere {
        return Err(invalid(format!(
            "{name} keeps its buffers or textures in other files. Export it as a .glb, which holds everything in one file."
        )));
    }
    if bin.is_none()
        && json["buffers"]
            .as_array()
            .is_some_and(|b| b.iter().any(|b| b["uri"].is_null()))
    {
        return Err(invalid(format!(
            "{name} is not a glTF model. Choose a .glb or .gltf file."
        )));
    }
    if json["meshes"].as_array().is_none_or(Vec::is_empty) {
        return Err(invalid(format!(
            "{name} has nothing to draw: it holds no meshes."
        )));
    }
    let needs = json["extensionsRequired"].as_array().into_iter().flatten();
    if let Some(extension) = needs
        .filter_map(Value::as_str)
        .find(|e| !DRAWN_EXTENSIONS.contains(e))
    {
        return Err(invalid(format!(
            "{name} needs {extension}, which UXDuck cannot draw yet. Export it again without it, such as without Draco or meshopt compression."
        )));
    }
    let detail = detail_of(&json, bin);
    if detail.texture_px > TEXTURE_MAX_PX {
        return Err(invalid(format!(
            "{name} has a {} px texture. UXDuck draws textures of up to {TEXTURE_MAX_PX} px, as some graphics cards hold none larger. Export its textures at {TEXTURE_ADVICE_PX} px.",
            detail.texture_px
        )));
    }
    if detail.texture_bytes > TEXTURE_MAX_BYTES {
        return Err(invalid(format!(
            "{name}'s textures take {} on the graphics card, enough to stop the maps drawing on a slower computer. UXDuck takes up to {}. Export fewer textures, at {TEXTURE_ADVICE_PX} px at most.",
            whole_megabytes(detail.texture_bytes),
            whole_megabytes(TEXTURE_MAX_BYTES)
        )));
    }
    Ok(warning(name, &detail))
}

/// A turn of 0, 90, 180 or 270 degrees.
///
/// # Errors
/// Any other.
pub(crate) fn turn(degrees: u16) -> Result<u16, ApiError> {
    if TURNS.contains(&degrees) {
        Ok(degrees)
    } else {
        Err(invalid("Turn a model by 0, 90, 180 or 270 degrees."))
    }
}

impl Engine {
    /// A file this connection uploaded, or one attached to a message it can
    /// see, checked and kept as a vehicle's own model. An upload used here
    /// goes once its copy is kept, as one taken out of the input does.
    pub(crate) fn keep_own_model(
        &mut self,
        connection: ConnectionId,
        attachment: &str,
        turn_deg: u16,
    ) -> Result<OwnModel, ApiError> {
        let found = self.attached_file(Some(connection), attachment)?;
        if found.bytes > OWN_MODEL_MAX {
            return Err(invalid(format!(
                "{} is {}. A model of your own is at most 32 MB.",
                found.name,
                megabytes(found.bytes)
            )));
        }
        let (found, bytes) = self.attached_bytes(Some(connection), attachment)?;
        let warning = check(&found.name, &bytes)?;
        let file = Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            });
        self.system
            .own_model_keep(&file, &bytes)
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        self.used_upload(connection, attachment);
        Ok(OwnModel {
            file,
            name: found.name,
            bytes: found.bytes,
            turn_deg,
            warning,
        })
    }

    /// A model no vehicle is drawn with any more is not kept.
    pub(crate) fn forget_own_model(&mut self, gone: Option<&OwnModel>) {
        let Some(gone) = gone else {
            return;
        };
        let used = self.state.vehicles.values().any(|v| {
            v.own_model
                .as_ref()
                .is_some_and(|own| own.file == gone.file)
        });
        if !used {
            let _ = self.system.own_model_remove(&gone.file);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use serde_json::json;

    use super::check;

    /// A binary glTF of this JSON and no buffer.
    fn glb(json: &serde_json::Value) -> Vec<u8> {
        glb_with(json, &[])
    }

    /// A binary glTF of this JSON and binary chunk, if not empty.
    fn glb_with(json: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
        let mut text = serde_json::to_vec(json).unwrap();
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let mut bin = bin.to_vec();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let total = 12 + 8 + text.len() + if bin.is_empty() { 0 } else { 8 + bin.len() };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"glTF");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
        bytes.extend_from_slice(&text);
        if !bin.is_empty() {
            bytes.extend_from_slice(&u32::try_from(bin.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(&0x004E_4942u32.to_le_bytes());
            bytes.extend_from_slice(&bin);
        }
        bytes
    }

    /// The first bytes of a PNG this wide and high, as far as its size.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    /// A model of this many triangles, placed by `placed` nodes, with one
    /// texture of this binary chunk's image.
    fn model(triangles: u64, placed: usize, image: &[u8]) -> Vec<u8> {
        let nodes: Vec<_> = (0..placed).map(|_| json!({ "mesh": 0 })).collect();
        let json = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": (0..placed).collect::<Vec<_>>() }],
            "nodes": nodes,
            "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 }, "indices": 1 }] }],
            "accessors": [{ "count": 3 }, { "count": triangles * 3 }],
            "buffers": [{ "byteLength": image.len() }],
            "bufferViews": [{ "buffer": 0, "byteLength": image.len() }],
            "images": [{ "bufferView": 0, "mimeType": "image/png" }]
        });
        glb_with(&json, image)
    }

    fn mesh() -> serde_json::Value {
        json!({ "asset": { "version": "2.0" }, "meshes": [{ "primitives": [] }] })
    }

    #[test]
    fn a_glb_and_a_self_contained_gltf_are_drawn() {
        check("plane.glb", &glb(&mesh())).unwrap();
        let mut inside = mesh();
        inside["buffers"] =
            json!([{ "byteLength": 4, "uri": "data:application/octet-stream;base64,AAAAAA==" }]);
        check("plane.gltf", &serde_json::to_vec(&inside).unwrap()).unwrap();
        let mut quantised = mesh();
        quantised["extensionsRequired"] = json!(["KHR_mesh_quantization"]);
        check("small.glb", &glb(&quantised)).unwrap();
    }

    #[test]
    fn what_drawing_a_model_costs_is_counted_from_its_file() {
        use super::{Detail, detail_of, image_size, json_of};
        let bytes = model(2_000, 4, &png(1024, 512));
        let (json, bin) = json_of("plane.glb", &bytes).unwrap();
        assert_eq!(
            detail_of(&json, bin),
            Detail {
                triangles: 8_000,
                parts: 4,
                texture_px: 1024,
                texture_bytes: 1024 * 512 * 16 / 3
            },
        );
        // A loop of nodes, which no exporter makes, ends the count.
        let mut looped = json.clone();
        looped["nodes"][0]["children"] = json!([0]);
        assert!(detail_of(&looped, bin).triangles <= 8_000);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0];
        jpeg.extend_from_slice(&[0xFF, 0xC2, 0, 17, 8, 0x0C, 0x00, 0x10, 0x00]);
        assert_eq!(image_size(&jpeg), Some((4096, 3072)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        webp.extend_from_slice(&[0xFF, 0x0F, 0, 0xFF, 0x07, 0]);
        assert_eq!(image_size(&webp), Some((4096, 2048)));
    }

    #[test]
    fn a_model_more_detailed_than_a_vehicle_needs_is_kept_with_a_warning() {
        assert_eq!(
            check("plane.glb", &model(8_000, 1, &png(1024, 1024))).unwrap(),
            None
        );
        let heavy = check("skywalker.glb", &model(300_000, 4, &png(4096, 4096)))
            .unwrap()
            .unwrap();
        assert_eq!(
            heavy,
            "skywalker.glb has 1.2 million triangles and textures that take 90 MB on the graphics card, more than a vehicle on a map needs, so the maps may slow down. Reduce its triangles with Blender's Decimate modifier and export fewer textures, at 2048 px at most."
        );
        assert!(!heavy.contains(';'));
        let wide = check("wide.glb", &model(10, 1, &png(16_384, 256))).unwrap_err();
        assert!(
            wide.message.contains("16384 px texture"),
            "{}",
            wide.message
        );
        let big = check("big.glb", &model(10, 1, &png(8192, 8192))).unwrap_err();
        assert!(
            big.message.contains("358 MB on the graphics card"),
            "{}",
            big.message
        );
        assert!(!big.message.contains(';'));
    }

    #[test]
    fn what_cannot_be_drawn_is_refused_in_words() {
        let refused = |name: &str, bytes: &[u8]| check(name, bytes).unwrap_err().message;
        assert!(refused("plane.stl", b"solid plane").contains("not a glTF model"));
        let mut old = mesh();
        old["asset"]["version"] = json!("1.0");
        assert!(refused("old.gltf", &serde_json::to_vec(&old).unwrap()).contains("glTF 1.0"));
        let mut cut = glb(&mesh());
        cut.truncate(cut.len() - 4);
        assert!(refused("cut.glb", &cut).contains("cut short"));
        let mut apart = mesh();
        apart["buffers"] = json!([{ "byteLength": 4, "uri": "plane.bin" }]);
        assert!(
            refused("apart.gltf", &serde_json::to_vec(&apart).unwrap())
                .contains("Export it as a .glb")
        );
        let mut draco = mesh();
        draco["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
        assert!(refused("draco.glb", &glb(&draco)).contains("KHR_draco_mesh_compression"));
        let empty = json!({ "asset": { "version": "2.0" } });
        assert!(refused("empty.glb", &glb(&empty)).contains("no meshes"));
        let big = vec![0u8; 33 * 1024 * 1024];
        assert!(refused("big.glb", &big).contains("at most 32 MB"));
    }
}
