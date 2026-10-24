//! `uxduck account watch` (S79): one of the account's computers, live
//! through the relay as someone watching from anywhere sees it, each value
//! with its age. The first surface of the fast path (ADR-0003 C8), until the
//! app shows remote vehicles (S86).

use std::collections::BTreeMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use uxduck_contracts::dto::edge::RemoteVehicle;

use crate::adapters::credentials::FileCredentials;
use crate::edge::now_ms;

/// One vehicle as `uxduck account watch` prints it: "Copter 1 · LOITER ·
/// armed · 30 m · 8.4 m/s · battery 44% · 0.4 s old".
#[must_use]
pub fn watch_line(vehicle: &RemoteVehicle, relay_now_ms: u64) -> String {
    let telemetry = vehicle.telemetry.as_ref();
    let item = |key: &str| {
        telemetry
            .and_then(|t| t.items.iter().find(|i| i.key == key))
            .and_then(|i| i.value)
    };
    let position = telemetry.and_then(|t| t.positions.first());
    let height = position.and_then(|p| {
        p.altitudes
            .iter()
            .find(|a| a.frame == uxduck_contracts::dto::telemetry::AltitudeFrame::AboveHome)
            .map(|a| a.meters)
    });
    let speed = item("speed.ground_mps").or_else(|| position.and_then(|p| p.ground_speed_mps));
    let mut parts = vec![vehicle.name.clone()];
    parts.extend(vehicle.mode.clone());
    parts.push(if vehicle.armed { "armed" } else { "disarmed" }.into());
    if let Some(m) = height {
        parts.push(format!("{m:.0} m"));
    }
    if let Some(mps) = speed {
        parts.push(format!("{mps:.1} m/s"));
    }
    if let Some(pct) = item("battery.remaining_pct") {
        parts.push(format!("battery {pct:.0}%"));
    }
    parts.extend(vehicle.notices.iter().take(1).cloned());
    #[allow(clippy::cast_precision_loss, reason = "an age in seconds, to a tenth")]
    let age = relay_now_ms.saturating_sub(vehicle.at_ms) as f64 / 1000.0;
    parts.push(format!("{age:.1} s old"));
    parts.join(" · ")
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One of the account's computers, opened as a viewer through the relay:
/// the one named, or the first online.
pub(crate) async fn open(
    data_dir: &std::path::Path,
    computer: Option<&str>,
) -> Result<(Socket, uxduck_contracts::dto::edge::Computer), String> {
    use uxduck_contracts::dto::edge::ComputerList;

    let api = crate::feedback::api_url().ok_or("this UXDuck has no account server")?;
    let token = FileCredentials::new(data_dir)
        .get(crate::account::TOKEN)
        .filter(|t| !t.is_empty())
        .ok_or("You're not signed in. Run `uxduck login` first.")?;
    let listed: ComputerList = crate::online::client(Duration::from_secs(20))
        .ok_or("UXDuck cannot make a web client here")?
        .get(format!("{api}/v1/computers"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|_| "UXDuck cannot reach its server")?
        .json()
        .await
        .map_err(|_| "the server's answer was not understood")?;
    let chosen = match computer {
        Some(wanted) => listed
            .computers
            .iter()
            .find(|c| c.id == wanted || c.name.eq_ignore_ascii_case(wanted)),
        None => listed.computers.iter().find(|c| c.online),
    }
    .ok_or_else(|| {
        let names: Vec<_> = listed.computers.iter().map(|c| c.name.as_str()).collect();
        format!(
            "no such computer online; the account has: {}",
            names.join(", ")
        )
    })?
    .clone();
    let base = api
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut request = format!("{base}/v1/computers/{}/live", chosen.id)
        .into_client_request()
        .map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| e.to_string())?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("could not watch {}: {e}", chosen.name))?;
    Ok((socket, chosen))
}

/// `uxduck account watch`: one of the account's computers, live, through the
/// relay as a viewer would see it (S79), each value with its age.
///
/// # Errors
/// Not signed in, no such computer, or the relay could not be reached.
#[allow(clippy::too_many_lines, reason = "a small viewer, read top to bottom")]
pub async fn watch(
    data_dir: &std::path::Path,
    computer: Option<&str>,
    vehicle: Option<&str>,
    once: bool,
) -> Result<(), String> {
    use uxduck_contracts::dto::edge::{EdgeDelta, EdgeSnapshot};

    let (socket, chosen) = open(data_dir, computer).await?;
    let (mut sink, mut stream) = socket.split();
    let mut vehicles: BTreeMap<String, RemoteVehicle> = BTreeMap::new();
    // The relay's clock minus this one's, from its last message.
    let mut offset: i64 = 0;
    let mut seq = 0u64;
    let mut online = chosen.online;
    let mut watching: Option<String> = None;
    let mut asked = 0u64;
    let mut print = tokio::time::interval(Duration::from_secs(1));
    let started = tokio::time::Instant::now();
    let relay_now =
        |offset: i64| u64::try_from(i64::try_from(now_ms()).unwrap_or(0) + offset).unwrap_or(0);
    loop {
        tokio::select! {
            _ = print.tick() => {
                if once && started.elapsed() < Duration::from_secs(3) { continue; }
                let now = relay_now(offset);
                println!("{}: {}", chosen.name, if online { "online" } else { "offline" });
                if vehicles.is_empty() {
                    println!("  no vehicles");
                }
                for vehicle in vehicles.values() {
                    println!("  {}", watch_line(vehicle, now));
                }
                if once { return Ok(()); }
            }
            frame = stream.next() => {
                let Some(Ok(Message::Text(text))) = frame else {
                    return Err(format!("the relay closed the connection to {}", chosen.name));
                };
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                let (params, method) = (message["params"].clone(), message["method"].as_str());
                let mut take = |snapshot: EdgeSnapshot| {
                    seq = snapshot.seq;
                    online = snapshot.computer.online;
                    offset = i64::try_from(snapshot.relay_at_ms).unwrap_or(0) - i64::try_from(now_ms()).unwrap_or(0);
                    vehicles = snapshot.vehicles.into_iter().map(|v| (v.id.clone(), v)).collect();
                };
                match method {
                    Some("edge/presence") => online = params["online"] == true,
                    Some("edge/snapshot") => {
                        if let Ok(snapshot) = serde_json::from_value(params) { take(snapshot); }
                    }
                    Some("edge/delta") => {
                        let Ok(delta) = serde_json::from_value::<EdgeDelta>(params) else { continue };
                        if delta.seq != seq + 1 {
                            // A gap: start again from a snapshot.
                            asked += 1;
                            let again = json!({ "jsonrpc": "2.0", "id": asked, "method": "edge/watch", "params": { "vehicleId": watching } });
                            let _ = sink.send(Message::text(again.to_string())).await;
                            continue;
                        }
                        seq = delta.seq;
                        offset = i64::try_from(delta.relay_at_ms).unwrap_or(0) - i64::try_from(now_ms()).unwrap_or(0);
                        for v in delta.vehicles { vehicles.insert(v.id.clone(), v); }
                        for id in delta.removed { vehicles.remove(&id); }
                    }
                    None if message["result"].is_object() => {
                        if let Ok(snapshot) = serde_json::from_value(message["result"].clone()) { take(snapshot); }
                    }
                    _ => {}
                }
                // Open the vehicle asked for, once it is known.
                if watching.is_none()
                    && let Some(wanted) = vehicle
                    && let Some(found) = vehicles.values().find(|v| v.id == wanted || v.name.eq_ignore_ascii_case(wanted))
                {
                    watching = Some(found.id.clone());
                    asked += 1;
                    let open = json!({ "jsonrpc": "2.0", "id": asked, "method": "edge/watch", "params": { "vehicleId": watching } });
                    let _ = sink.send(Message::text(open.to_string())).await;
                }
            }
        }
    }
}

type Frames = futures::stream::SplitStream<Socket>;

/// The relay's next message.
async fn next_frame(stream: &mut Frames, computer: &str) -> Result<Value, String> {
    loop {
        let Some(Ok(Message::Text(text))) = stream.next().await else {
            return Err(format!("the relay closed the connection to {computer}"));
        };
        if let Ok(frame) = serde_json::from_str::<Value>(&text) {
            return Ok(frame);
        }
    }
}

/// The relay's answer to request `id`.
async fn answer(stream: &mut Frames, computer: &str, id: u64) -> Result<Value, String> {
    loop {
        let frame = next_frame(stream, computer).await?;
        if frame["id"] == id {
            return Ok(frame);
        }
    }
}

/// `uxduck account command`: one command to a vehicle on another of the
/// account's computers, through the relay (S80), with its receipt. With
/// `take_control`, the vehicle's control lease first; it ends with this.
///
/// # Errors
/// Not signed in, no such computer or vehicle, refused, or the receipt says
/// it did not happen.
pub async fn command(
    data_dir: &std::path::Path,
    computer: &str,
    vehicle: &str,
    command: Value,
    take_control: bool,
) -> Result<(), String> {
    let (socket, chosen) = open(data_dir, Some(computer)).await?;
    let (mut sink, mut stream) = socket.split();
    // The first snapshot names the vehicles.
    let found = loop {
        let frame = next_frame(&mut stream, &chosen.name).await?;
        if frame["method"] != "edge/snapshot" {
            continue;
        }
        let vehicles = frame["params"]["vehicles"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        break vehicles.into_iter().find(|v| {
            v["id"] == vehicle
                || v["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(vehicle))
        });
    };
    // A vehicle the list has not reached yet is taken by its id as given.
    let (vehicle_id, name) = found.map_or_else(
        || (vehicle.to_owned(), vehicle.to_owned()),
        |v| {
            (
                v["id"].as_str().unwrap_or(vehicle).to_owned(),
                v["name"].as_str().unwrap_or(vehicle).to_owned(),
            )
        },
    );
    let ask = |id: u64, method: &str, params: Value| {
        Message::text(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        )
    };
    let refused = |frame: &Value| {
        frame.get("error").map(|e| {
            format!(
                "{} ({})",
                e["message"].as_str().unwrap_or("refused"),
                e["data"]["kind"].as_str().unwrap_or("error")
            )
        })
    };
    if take_control {
        sink.send(ask(
            1,
            "edge/lease",
            json!({ "vehicleId": vehicle_id, "take": true }),
        ))
        .await
        .map_err(|e| e.to_string())?;
        let lease = answer(&mut stream, &chosen.name, 1).await?;
        if let Some(why) = refused(&lease) {
            return Err(format!("could not take control of {name}: {why}"));
        }
        eprintln!("You have control of {name}.");
        if let Some(failsafe) = lease["result"]["failsafe"].as_str() {
            eprintln!("{failsafe}");
        }
    }
    let mut id_bytes = [0u8; 8];
    let _ = getrandom::fill(&mut id_bytes);
    let command_id = id_bytes.iter().fold(String::new(), |mut id, b| {
        use std::fmt::Write as _;
        let _ = write!(id, "{b:02x}");
        id
    });
    sink.send(ask(
        2,
        "edge/command",
        json!({ "commandId": command_id, "vehicleId": vehicle_id, "command": command }),
    ))
    .await
    .map_err(|e| e.to_string())?;
    let sent = answer(&mut stream, &chosen.name, 2).await?;
    if let Some(why) = refused(&sent) {
        return Err(format!("not sent: {why}; its id was {command_id}"));
    }
    let receipt = &sent["result"];
    let status = receipt["status"].as_str().unwrap_or("unknown");
    let detail = receipt["detail"]
        .as_str()
        .map(|d| format!(": {d}"))
        .unwrap_or_default();
    println!("{name}: {status}{detail}");
    if status == "accepted" {
        Ok(())
    } else {
        Err(format!("{name}: {status}{detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_contracts::dto::telemetry::{
        Altitude, AltitudeFrame, PositionEstimate, TelemetryItem, Unit, VehicleTelemetry,
    };

    #[test]
    fn a_vehicle_reads_as_one_line_with_its_age() {
        let item = |key: &str, value: f64| TelemetryItem {
            key: key.into(),
            label: key.into(),
            unit: Unit::None,
            value: Some(value),
            text: None,
            rate_per_s: None,
            source: "mavlink".into(),
            component_id: 1,
            time_us: 1,
        };
        let vehicle = RemoteVehicle {
            id: "v1".into(),
            name: "Copter 1".into(),
            vehicle_type: "Quadrotor".into(),
            autopilot: "Ardupilotmega".into(),
            size_cm: None,
            look: None,
            presence: uxduck_contracts::dto::vehicle::Presence::Online,
            armed: true,
            mode: Some("LOITER".into()),
            telemetry: Some(VehicleTelemetry {
                vehicle_id: "v1".into(),
                positions: vec![PositionEstimate {
                    source: "fused".into(),
                    component_id: 1,
                    latitude_deg: 0.0,
                    longitude_deg: 0.0,
                    altitudes: vec![Altitude {
                        meters: 30.2,
                        frame: AltitudeFrame::AboveHome,
                    }],
                    heading_deg: None,
                    course_deg: None,
                    ground_speed_mps: Some(8.37),
                    climb_mps: None,
                    horizontal_accuracy_m: None,
                    vertical_accuracy_m: None,
                    time_us: 1,
                }],
                items: vec![item("battery.remaining_pct", 44.0)],
            }),
            notices: vec!["Battery low".into()],
            lease: None,
            cameras: Vec::new(),
            at_ms: 10_000,
        };
        assert_eq!(
            watch_line(&vehicle, 10_400),
            "Copter 1 · LOITER · armed · 30 m · 8.4 m/s · battery 44% · Battery low · 0.4 s old"
        );
    }
}
