// The example plugin (plugins/home-distance) as one TypeScript file, with
// no build step (ADR-0006 X7): positions and battery items in; the distance
// from home, a low-battery notice and a Mark command out.

import type {
  CommandCall,
  Description,
  Effects,
  ItemBatch,
  MessageBatch,
  ProducedItem,
  PluginNotification,
} from "@uxduck/sdk";
import { circle, notify, value } from "@uxduck/sdk";

const LOW_PCT = 20;
const RESET_PCT = 25;

interface Vehicle {
  home?: [number, number];
  warned: boolean;
}

const vehicles = new Map<string, Vehicle>();

function vehicle(id: string): Vehicle {
  let found = vehicles.get(id);
  if (!found) {
    found = { warned: false };
    vehicles.set(id, found);
  }
  return found;
}

/** The distance between two points on the earth, in metres. */
function distanceM(from: [number, number], to: [number, number]): number {
  const rad = (d: number) => (d * Math.PI) / 180;
  const a =
    Math.sin(rad(to[0] - from[0]) / 2) ** 2 +
    Math.cos(rad(from[0])) * Math.cos(rad(to[0])) * Math.sin(rad(to[1] - from[1]) / 2) ** 2;
  return 2 * 6371000 * Math.asin(Math.sqrt(a));
}

export function describe(): Description {
  return {
    panels: [
      {
        id: "home",
        title: "Home",
        widgets: [
          { type: "value", item: "home.distance_m", label: "From home" },
          { type: "button", label: "Mark", command: "mark" },
        ],
      },
    ],
  };
}

export function onMessages(batch: MessageBatch): Effects {
  const items: ProducedItem[] = [];
  const features = [];
  for (const message of batch.messages) {
    // `lat` and `lon` are in degE7, as MAVLink sends them.
    const here: [number, number] = [
      (message.fields.lat as number) / 1e7,
      (message.fields.lon as number) / 1e7,
    ];
    const state = vehicle(message.vehicleId);
    if (!state.home) {
      state.home = here;
      features.push({ ...circle("home-circle", here[0], here[1], 100), fill: true });
    }
    items.push(
      value(
        message.vehicleId,
        "home.distance_m",
        "From home",
        "meter",
        distanceM(state.home, here),
      ),
    );
  }
  return { items, features };
}

export function onItems(batch: ItemBatch): Effects {
  const notifications: PluginNotification[] = [];
  for (const item of batch.items) {
    if (item.key !== "battery.remaining_pct" || item.value == null) continue;
    const state = vehicle(item.vehicleId);
    if (item.value < LOW_PCT && !state.warned) {
      state.warned = true;
      notifications.push(
        notify(item.vehicleId, "warning", `Battery at ${Math.floor(item.value)} %`),
      );
    } else if (item.value > RESET_PCT) {
      state.warned = false;
    }
  }
  return { notifications };
}

export function onCommand(call: CommandCall): Effects {
  if (call.command !== "mark") return { log: [`unknown command ${call.command}`] };
  const label = String((call.params as { label?: string }).label ?? "Mark").slice(0, 80);
  return { notifications: [notify(call.vehicleId, "info", `Marked: ${label}`)] };
}
