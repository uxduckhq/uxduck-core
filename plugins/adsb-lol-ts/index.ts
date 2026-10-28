/**
 * Traffic from adsb.lol (S171, ADR-0034): aircraft around the vehicles, from
 * this computer, within adsb.lol's free limits. Off until a person turns it
 * on, since it sends the vehicles' area to adsb.lol.
 *
 * Measured on 2026-10-21: `/v2/point/{lat}/{lon}/{radius_nm}` answers without
 * a key, allows 4 requests a second, 60 a minute and 500 a day from one
 * address (`x-ratelimit-limit: 4, 4;w=1, 60;w=60, 500;w=86400`), and its
 * `x-ratelimit-remaining` counts only the second, so the day is counted here.
 */

import type {
  Effects,
  FetchedInput,
  MessageBatch,
  PluginTraffic,
  PluginTrafficStatus,
  StartInput,
  TickInput,
} from "@uxduck/sdk";

const API = "https://api.adsb.lol/v2/point";
const CREDIT = "adsb.lol (ODbL)";
/** Requests a day without a key, unless adsb.lol says otherwise. */
const DAY_LIMIT = 500;
/** The default explicit polling interval; arming never changes it. */
const DEFAULT_REFRESH_S = 30;
/** A vehicle not heard for this long is not looked around. */
const POSITION_FRESH_MS = 60_000;
/** Beyond the vehicles, and at most, as adsb.lol allows. */
const MARGIN_NM = 25;
const MOST_NM = 250;
const FEET = 0.3048;
const KNOTS = 0.514_444;

interface Seen {
  lat: number;
  lon: number;
  atMs: number;
}

const vehicles = new Map<string, Seen>();
let day = "";
let used = 0;
let limit = DAY_LIMIT;
/** No more requests until then: the day's limit, or adsb.lol said so. */
let blockedUntilMs = 0;
let lastAskedMs = 0;
let asking = false;
let nowMs = 0;

export function onStart(input: StartInput): Effects {
  nowMs = input.nowMs;
  const kept = input.stored as {
    day?: string;
    used?: number;
    limit?: number;
    blockedUntilMs?: number;
  };
  day = kept.day ?? "";
  used = kept.used ?? 0;
  limit = kept.limit ?? DAY_LIMIT;
  blockedUntilMs = kept.blockedUntilMs ?? 0;
  return { trafficStatus: status() };
}

export function onMessages(batch: MessageBatch): Effects {
  for (const m of batch.messages) {
    const seen = vehicles.get(m.vehicleId) ?? { lat: 0, lon: 0, atMs: 0 };
    if (m.name === "GLOBAL_POSITION_INT") {
      const lat = Number(m.fields.lat) / 1e7;
      const lon = Number(m.fields.lon) / 1e7;
      // 0, 0 is no position (no fix).
      if (Number.isFinite(lat) && Number.isFinite(lon) && (lat !== 0 || lon !== 0)) {
        seen.lat = lat;
        seen.lon = lon;
        seen.atMs = nowMs || Date.now();
      }
    }
    vehicles.set(m.vehicleId, seen);
  }
  return {};
}

export function onTick(input: TickInput): Effects {
  nowMs = input.nowMs;
  newDay();
  const around = input.traffic
    ? input.traffic.references.map((r) => ({
        lat: r.latitudeDeg,
        lon: r.longitudeDeg,
        atMs: nowMs,
      }))
    : [...vehicles.values()].filter((v) => v.atMs && nowMs - v.atMs < POSITION_FRESH_MS);
  if (around.length === 0)
    return { trafficStatus: status("Waiting for a selected location's position") };
  if (nowMs < blockedUntilMs) return { trafficStatus: status() };
  const every = (input.traffic?.refreshS ?? DEFAULT_REFRESH_S) * 1000;
  if (asking || nowMs - lastAskedMs < every) return {};
  const coverage = area(around, input.traffic ? input.traffic.radiusM / 1852 : MARGIN_NM);
  if (coverage.radiusNm > MOST_NM) {
    return {
      trafficStatus: status(
        "Selected areas exceed the feed's 250 NM request radius; coverage unavailable. Reduce the range or choose closer locations.",
        true,
      ),
    };
  }
  asking = true;
  lastAskedMs = nowMs;
  used += 1;
  const { lat, lon, radiusNm } = coverage;
  return {
    fetch: [{ id: "around", url: `${API}/${lat.toFixed(4)}/${lon.toFixed(4)}/${radiusNm}` }],
    store: kept(),
    trafficStatus: status(),
  };
}

export function onFetched(input: FetchedInput): Effects {
  asking = false;
  nowMs = input.nowMs;
  if (input.error) return { trafficStatus: status(`Cannot reach adsb.lol: ${input.error}`, true) };
  const headers = input.headers ?? {};
  const daily = /(\d+);w=86400/.exec(headers["x-ratelimit-limit"] ?? "");
  if (daily) limit = Number(daily[1]);
  if (input.status === 429) {
    // Told to stop: until the time it gives, else the next UTC day.
    const after = Number(headers["retry-after"]);
    blockedUntilMs = Number.isFinite(after) && after > 0 ? nowMs + after * 1000 : nextDayMs();
    return { store: kept(), trafficStatus: status() };
  }
  if (input.status !== 200)
    return { trafficStatus: status(`adsb.lol answered ${input.status}`, true) };
  let aircraft: Aircraft[];
  try {
    aircraft = (JSON.parse(input.body ?? "") as { ac?: Aircraft[] }).ac ?? [];
  } catch {
    return { trafficStatus: status("adsb.lol sent something that is not JSON", true) };
  }
  return {
    traffic: aircraft.filter((a) => /^[0-9a-f]{6}$/i.test(a.hex)).map(traffic),
    trafficStatus: status(),
  };
}

/** One aircraft as adsb.lol (readsb) gives it. */
interface Aircraft {
  hex: string;
  flight?: string;
  squawk?: string;
  category?: string;
  lat?: number;
  lon?: number;
  alt_geom?: number;
  alt_baro?: number | "ground";
  track?: number;
  gs?: number;
  geom_rate?: number;
  baro_rate?: number;
  seen?: number;
  seen_pos?: number;
}

function traffic(a: Aircraft): PluginTraffic {
  const geometric = a.alt_geom !== undefined;
  const feet = geometric ? a.alt_geom : typeof a.alt_baro === "number" ? a.alt_baro : undefined;
  const rate = a.geom_rate ?? a.baro_rate;
  return {
    address: a.hex,
    callsign: a.flight?.trim() || undefined,
    squawk: a.squawk,
    category: category(a.category),
    latitudeDeg: a.lat,
    longitudeDeg: a.lon,
    altitudeM: feet === undefined ? undefined : feet * FEET,
    altitudeFrame: feet === undefined ? undefined : geometric ? "ellipsoid" : "pressure",
    trackDeg: a.track,
    groundSpeedMps: a.gs === undefined ? undefined : a.gs * KNOTS,
    climbMps: rate === undefined ? undefined : (rate * FEET) / 60,
    seenAgoS: a.seen_pos ?? a.seen,
  };
}

/** ADS-B's emitter category, as letters and a digit. */
function category(code: string | undefined): PluginTraffic["category"] {
  const names: Record<string, PluginTraffic["category"]> = {
    A1: "light",
    A2: "small",
    A3: "large",
    A4: "large",
    A5: "heavy",
    A6: "fast",
    A7: "rotorcraft",
    B1: "glider",
    B2: "balloon",
    B3: "parachute",
    B4: "ultralight",
    B6: "uav",
    B7: "space",
    C1: "surface",
    C2: "surface",
    C3: "obstacle",
  };
  return (code && names[code]) || "unknown";
}

/** A circle round every vehicle, MARGIN_NM beyond the farthest, at most MOST_NM. */
export function area(
  around: Seen[],
  marginNm = MARGIN_NM,
): { lat: number; lon: number; radiusNm: number } {
  const lat = Number((around.reduce((sum, v) => sum + v.lat, 0) / around.length).toFixed(4));
  // Circular longitude mean handles locations either side of the date line.
  const sin = around.reduce((sum, v) => sum + Math.sin((v.lon * Math.PI) / 180), 0);
  const cos = around.reduce((sum, v) => sum + Math.cos((v.lon * Math.PI) / 180), 0);
  const lon = Number(((Math.atan2(sin, cos) * 180) / Math.PI).toFixed(4));
  const farthest = Math.max(...around.map((v) => nauticalMiles(lat, lon, v.lat, v.lon)));
  return { lat, lon, radiusNm: Math.ceil(farthest + marginNm) };
}

function nauticalMiles(lat1: number, lon1: number, lat2: number, lon2: number): number {
  const rad = Math.PI / 180;
  const h =
    Math.sin(((lat2 - lat1) * rad) / 2) ** 2 +
    Math.cos(lat1 * rad) * Math.cos(lat2 * rad) * Math.sin(((lon2 - lon1) * rad) / 2) ** 2;
  return 2 * 3440.065 * Math.asin(Math.sqrt(Math.min(1, h)));
}

/** A new UTC day starts the count again. */
function newDay() {
  const today = new Date(nowMs).toISOString().slice(0, 10);
  if (today !== day) {
    day = today;
    used = 0;
    if (blockedUntilMs <= nowMs) blockedUntilMs = 0;
  }
}

function nextDayMs(): number {
  const next = new Date(nowMs);
  next.setUTCHours(24, 0, 0, 0);
  return next.getTime();
}

function kept() {
  return { day, used, limit, blockedUntilMs };
}

function status(note?: string, problem = false): PluginTrafficStatus {
  if (nowMs < blockedUntilMs) {
    const until = new Date(blockedUntilMs).toISOString().slice(11, 16);
    return {
      note: `adsb.lol's free limit is used up. Asking again at ${until} UTC.`,
      problem: true,
      credit: CREDIT,
    };
  }
  if (used >= limit) {
    blockedUntilMs = nextDayMs();
    return status();
  }
  const today = `${used} of ${limit} requests today`;
  return { note: note ? `${note} · ${today}` : today, problem, credit: CREDIT };
}
