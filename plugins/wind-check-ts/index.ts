// A preflight check from a plugin (S194, ADR-0038 K12): the wind is under
// 8 m/s, from the vehicle's own estimate (`wind.speed_mps`, from ArduPilot's
// WIND message) or a speed a person set by hand. It reports the check for
// each vehicle it hears about whenever the wind comes in, and asks to be
// woken again in 10 s to report anyway, so its report never goes stale
// while it runs (its staleMs is 30 s). Its words carry no live numbers, so the
// vehicle's list is not kept again for every reading; the speed is an item.

import type { CommandCall, Description, Effects, ItemBatch, ReportedCheck } from "@uxduck/sdk";
import { check, text, value } from "@uxduck/sdk";

const LIMIT_MPS = 8;
/** Reports again this soon when nothing else comes. */
const AGAIN_MS = 10_000;

interface Wind {
  /** The vehicle's latest estimate, in m/s. */
  estimate?: number;
  /** A speed a person set, in m/s; it counts instead of the estimate. */
  byHand?: number;
}

const vehicles = new Map<string, Wind>();

function wind(id: string): Wind {
  let found = vehicles.get(id);
  if (!found) {
    found = {};
    vehicles.set(id, found);
  }
  return found;
}

/** Where the check stands for a vehicle, and the items the panel shows. */
function report(vehicleId: string): Effects {
  const { estimate, byHand } = wind(vehicleId);
  const speed = byHand ?? estimate;
  const checks: ReportedCheck[] = [
    speed === undefined
      ? check(vehicleId, "wind", "waiting", "No wind estimate from the vehicle yet.")
      : speed < LIMIT_MPS
        ? check(vehicleId, "wind", "pass", `Under ${LIMIT_MPS} m/s.`)
        : check(vehicleId, "wind", "fail", `${LIMIT_MPS} m/s or more.`),
  ];
  const items = [
    text(
      vehicleId,
      "wind_check.source",
      "Wind from",
      byHand !== undefined ? "Set by hand" : estimate !== undefined ? "The vehicle" : "Nothing yet",
    ),
  ];
  if (speed !== undefined) {
    items.push(value(vehicleId, "wind_check.speed_mps", "Wind", "meter-per-second", speed));
  }
  return { checks, items };
}

function merged(all: Effects[]): Effects {
  return {
    checks: all.flatMap((e) => e.checks ?? []),
    items: all.flatMap((e) => e.items ?? []),
    wakeInMs: AGAIN_MS,
  };
}

export function describe(): Description {
  return {
    panels: [
      {
        id: "wind",
        title: "Wind check",
        widgets: [
          { type: "value", item: "wind_check.speed_mps", label: "Wind" },
          { type: "value", item: "wind_check.source", label: "From" },
          { type: "button", label: "5 m/s", command: "set-wind", params: { speedMps: 5 } },
          { type: "button", label: "12 m/s", command: "set-wind", params: { speedMps: 12 } },
          {
            type: "button",
            label: "Use the vehicle's",
            command: "use-estimate",
            help: "Checks the wind the vehicle estimates again, instead of the speed set by hand.",
          },
        ],
      },
    ],
  };
}

export function onItems(batch: ItemBatch): Effects {
  const heard = new Set<string>();
  for (const item of batch.items) {
    if (item.key !== "wind.speed_mps") continue;
    heard.add(item.vehicleId);
    wind(item.vehicleId).estimate = item.value ?? undefined;
  }
  return merged([...heard].map(report));
}

/** Woken with nothing new: every vehicle heard about, again. */
export function onTick(): Effects {
  return merged([...vehicles.keys()].map(report));
}

export function onCommand(call: CommandCall): Effects {
  if (!call.vehicleId) return {};
  const params = call.params as { speedMps?: number };
  if (call.command === "set-wind" && typeof params.speedMps === "number") {
    wind(call.vehicleId).byHand = params.speedMps;
  } else if (call.command === "use-estimate") {
    wind(call.vehicleId).byHand = undefined;
  }
  return merged([report(call.vehicleId)]);
}
