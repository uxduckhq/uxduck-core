//! `uxduck plugin new <id> --typescript` (ADR-0006 X7): a plugin that
//! runs, with a test that passes, as a place to start.

use std::path::Path;

/// The key group a plugin's items use: its id with dashes as underscores.
fn group(id: &str) -> String {
    id.replace('-', "_")
}

fn title(id: &str) -> String {
    let words = id.replace('-', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn manifest(id: &str) -> String {
    let manifest = serde_json::json!({
        "abi": 2,
        "id": id,
        "name": title(id),
        "version": "0.1.0",
        "description": "(what it does, in a sentence)",
        "script": "index.ts",
        "capabilities": ["read"],
        "subscribe": { "items": { "keys": ["battery.*"], "cadenceMs": 1000 } }
    });
    serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n"
}

fn script(id: &str) -> String {
    let group = group(id);
    let name = title(id);
    format!(
        r#"// {name}: a UXDuck plugin in TypeScript. UXDuck strips the types and runs
// it in its sandbox; there is no build step. Try it with
//
//   uxduck plugin test .
//
// which runs it against test.tlog and checks test.json, then install it
// from the Plugins page or with `uxduck plugin install .`.

import type {{ Description, Effects, ItemBatch, ProducedItem }} from "@uxduck/sdk";
import {{ latest, value }} from "@uxduck/sdk";

// What it shows in the Vehicle tab.
export function describe(): Description {{
  return {{
    panels: [
      {{
        id: "main",
        title: "{name}",
        widgets: [{{ type: "value", item: "{group}.battery_pct", label: "Battery" }}],
      }},
    ],
  }};
}}

// The items plugin.json subscribes to, the latest of each, once a second.
export function onItems(batch: ItemBatch): Effects {{
  const items: ProducedItem[] = [];
  for (const vehicleId of new Set(batch.items.map((item) => item.vehicleId))) {{
    const remaining = latest(batch, vehicleId, "battery.remaining_pct")?.value;
    if (remaining != null) {{
      // A key ends in its unit: _pct for percent, _m for metres, and so on.
      items.push(value(vehicleId, "{group}.battery_pct", "Battery", "percent", remaining));
    }}
  }}
  return {{ items }};
}}
"#
    )
}

fn test(id: &str) -> String {
    let test = serde_json::json!({ "expect": { "items": [format!("{}.battery_pct", group(id))] } });
    serde_json::to_string_pretty(&test).unwrap_or_default() + "\n"
}

/// A short flight of the development vehicle, to test against.
fn tlog() -> Vec<u8> {
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    // Nobody asks for its streams here: it sends them all.
    fake.streams_on_request = false;
    let mut log = Vec::new();
    for step in 0..30u32 {
        let at_us = u64::from(step) * 1_000_000;
        let _ = uxduck_mavlink::tlog::record(&mut log, at_us, &fake.heartbeat());
        for frame in fake.telemetry(f64::from(step)) {
            let _ = uxduck_mavlink::tlog::record(&mut log, at_us, &frame);
        }
    }
    log
}

/// Writes the plugin into `dir`, which must not exist yet.
///
/// # Errors
/// An id that is not lower case letters, digits and dashes, a folder that
/// is already there, or one that cannot be written.
pub fn write(id: &str, dir: &Path) -> Result<(), String> {
    if !uxduck_core::domain::plugin::valid_id(id) {
        return Err(format!(
            "{id:?}: a plugin id is lower case letters, digits and dashes"
        ));
    }
    if dir.exists() {
        return Err(format!("{} is already there", dir.display()));
    }
    let write = |name: &str, bytes: &[u8]| {
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("{name}: {e}"))
    };
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write("plugin.json", manifest(id).as_bytes())?;
    write("index.ts", script(id).as_bytes())?;
    write("test.json", test(id).as_bytes())?;
    write("test.tlog", &tlog())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_id() {
        assert_eq!(group("flight-time"), "flight_time");
        assert_eq!(title("flight-time"), "Flight time");
        assert!(script("flight-time").contains(r#""flight_time.battery_pct""#));
        assert!(write("Not Valid", Path::new("/nowhere")).is_err());
    }
}
