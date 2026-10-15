//! Generates a type for every message of the ArduPilot dialect (ADR-0006
//! X3): MAVLink's own names, each field documented with its unit, and the
//! units as data, so a plugin never reads a byte layout.

use std::fmt::Write as _;

use mavinspect::protocol::{MavType, Message};

const RUST_KEYWORDS: [&str; 12] = [
    "type", "mod", "match", "in", "use", "ref", "move", "loop", "fn", "impl", "struct", "enum",
];

fn camel(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            let mut chars = lower.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect()
}

fn scalar(kind: &MavType) -> &'static str {
    match kind {
        MavType::UInt8 | MavType::UInt8MavlinkVersion | MavType::Char => "u8",
        MavType::UInt16 => "u16",
        MavType::UInt32 => "u32",
        MavType::UInt64 => "u64",
        MavType::Int8 => "i8",
        MavType::Int16 => "i16",
        MavType::Int32 => "i32",
        MavType::Int64 => "i64",
        MavType::Float => "f32",
        MavType::Double => "f64",
        MavType::Array(..) => "()",
    }
}

/// The Rust type, and the deserializer it needs when MAVLink's NaN arrives
/// as JSON null.
fn rust_type(kind: &MavType) -> (String, Option<&'static str>) {
    match kind {
        MavType::Array(inner, _) if **inner == MavType::Char => ("String".into(), None),
        MavType::Array(inner, _) if matches!(**inner, MavType::Float | MavType::Double) => (
            format!("Vec<{}>", scalar(inner)),
            Some("crate::messages::floats"),
        ),
        MavType::Array(inner, _) => (format!("Vec<{}>", scalar(inner)), None),
        MavType::Float | MavType::Double => (scalar(kind).into(), Some("crate::messages::float")),
        other => (scalar(other).into(), None),
    }
}

fn doc(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('[', "(")
        .replace(']', ")")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn message(out: &mut String, message: &Message) {
    let name = camel(message.name());
    let _ = writeln!(
        out,
        "/// `{}` (id {}): {}",
        message.name(),
        message.id(),
        doc(message.description())
    );
    let _ = writeln!(
        out,
        "#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]\n#[serde(default)]\npub struct {name} {{"
    );
    let mut units = Vec::new();
    for field in message.fields() {
        let (kind, from) = rust_type(field.r#type());
        let unit = field.units().map(ToString::to_string);
        let described = doc(field.description());
        match &unit {
            Some(unit) => {
                let _ = writeln!(out, "    /// {described} Unit: `{unit}`.");
                units.push((field.name().to_owned(), unit.clone()));
            }
            None => {
                let _ = writeln!(out, "    /// {described}");
            }
        }
        if let Some(from) = from {
            let _ = writeln!(out, "    #[serde(deserialize_with = \"{from}\")]");
        }
        let ident = if RUST_KEYWORDS.contains(&field.name()) {
            format!("r#{}", field.name())
        } else {
            field.name().to_owned()
        };
        let _ = writeln!(out, "    pub {ident}: {kind},");
    }
    let _ = writeln!(out, "}}\n");
    let listed: Vec<String> = units
        .iter()
        .map(|(field, unit)| format!("({field:?}, {unit:?})"))
        .collect();
    let _ = writeln!(
        out,
        "impl crate::Message for {name} {{\n    const NAME: &'static str = {:?};\n    const ID: u32 = {};\n    const UNITS: &'static [(&'static str, &'static str)] = &[{}];\n}}\n",
        message.name(),
        message.id(),
        listed.join(", ")
    );
}

fn main() {
    let Some(dialect) = mavio::definitions::protocol().get_dialect_by_name("ardupilotmega") else {
        panic!("the ArduPilot dialect's definitions are missing");
    };
    let mut messages: Vec<&Message> = dialect.messages().into_iter().collect();
    messages.sort_by_key(|m| m.name().to_owned());
    let mut out = String::new();
    for found in &messages {
        message(&mut out, found);
    }
    // For the SDK's own test: every message read as its type and written
    // back.
    out.push_str("#[doc(hidden)]\npub fn round_trip(name: &str, fields: serde_json::Map<String, serde_json::Value>) -> Option<serde_json::Value> {\n    let value = serde_json::Value::Object(fields);\n    match name {\n");
    for found in &messages {
        let _ = writeln!(
            out,
            "        {:?} => serde_json::from_value::<{}>(value).ok().and_then(|m| serde_json::to_value(m).ok()),",
            found.name(),
            camel(found.name())
        );
    }
    out.push_str("        _ => None,\n    }\n}\n");
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into()))
        .join("messages.rs");
    std::fs::write(&path, out).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    println!("cargo::rerun-if-changed=build.rs");
}
