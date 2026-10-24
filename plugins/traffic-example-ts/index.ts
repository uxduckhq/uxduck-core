/**
 * An example traffic source (ADR-0034, ADR-0006 X30 and X31): every 5 s it
 * fetches readsb's `aircraft.json` from this computer and reports the
 * aircraft in it, which join the Airspace tab while the plugin is on. A
 * vendor's feed is the same shape: name its host in `plugin.json`, fetch,
 * and turn its answer into `traffic`.
 */

import type { Effects, FetchedInput, PluginTraffic } from "@uxduck/sdk";

const URL = "http://localhost:8080/data/aircraft.json";
const FEET = 0.3048;
const KNOTS = 0.514_444;

/** One aircraft as readsb writes it. */
interface Readsb {
  hex: string;
  flight?: string;
  squawk?: string;
  lat?: number;
  lon?: number;
  alt_geom?: number;
  track?: number;
  gs?: number;
  geom_rate?: number;
  seen?: number;
}

export function onStart(): Effects {
  return { fetch: [{ id: "aircraft", url: URL }] };
}

export function onTick(): Effects {
  return { fetch: [{ id: "aircraft", url: URL }] };
}

export function onFetched(input: FetchedInput): Effects {
  if (input.error || input.status !== 200) {
    return { log: [`${URL}: ${input.error ?? `status ${input.status}`}`] };
  }
  const aircraft = JSON.parse(input.body ?? "") as { aircraft?: Readsb[]; ac?: Readsb[] };
  const list = aircraft.aircraft ?? aircraft.ac ?? [];
  return { traffic: list.filter((a) => /^[0-9a-f]{6}$/i.test(a.hex)).map(traffic) };
}

function traffic(a: Readsb): PluginTraffic {
  return {
    address: a.hex,
    callsign: a.flight?.trim(),
    squawk: a.squawk,
    latitudeDeg: a.lat,
    longitudeDeg: a.lon,
    altitudeM: a.alt_geom === undefined ? undefined : a.alt_geom * FEET,
    altitudeFrame: a.alt_geom === undefined ? undefined : "geometric",
    trackDeg: a.track,
    groundSpeedMps: a.gs === undefined ? undefined : a.gs * KNOTS,
    climbMps: a.geom_rate === undefined ? undefined : (a.geom_rate * FEET) / 60,
    seenAgoS: a.seen,
  };
}
