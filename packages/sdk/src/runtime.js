// The helpers a TypeScript plugin imports from "@uxduck/sdk" (ADR-0006 X7).
// Plain JavaScript, so UXDuck's script host runs this very file as the
// module "@uxduck/sdk"; `index.ts` gives it its types. Keep it small and
// free of imports.

/** Who an item or a notice is about: a vehicle, or with `null` the plugin itself (its tabs). */
const about = (vehicleId) => (vehicleId === null || vehicleId === undefined ? {} : { vehicleId });

/**
 * A numeric telemetry item; its key ends in its unit's suffix, such as
 * `home.distance_m`. With `null` for the vehicle it is the plugin's own,
 * shown in its tabs, such as `stopwatch.elapsed_s`.
 */
export function value(vehicleId, key, label, unit, number) {
  return { ...about(vehicleId), key, label, unit, value: number };
}

/** A text telemetry item; with `null` for the vehicle, the plugin's own. */
export function text(vehicleId, key, label, words) {
  return { ...about(vehicleId), key, label, unit: "none", text: words };
}

/** Rows for a `table` widget: an item whose text holds them. */
export function table(vehicleId, key, label, rows) {
  return text(vehicleId, key, label, JSON.stringify(rows));
}

/** A notification under the plugin's name, about a vehicle or, with `null`, about nothing in particular. */
export function notify(vehicleId, severity, title, body) {
  const notice = { ...about(vehicleId), severity, title };
  return body === undefined ? notice : { ...notice, body };
}

/** A `MAV_CMD` for a vehicle, by number. */
export function commandLong(vehicleId, command, params) {
  return { vehicleId, command: { type: "long", command, params } };
}

/** A point on the map, at a latitude and longitude in degrees. */
export function point(id, latitudeDeg, longitudeDeg) {
  return { id, geometry: { type: "Point", coordinates: [longitudeDeg, latitudeDeg] } };
}

/** A circle on the map: 64 sides around a centre in degrees, `radiusM` metres across the ground. */
export function circle(id, latitudeDeg, longitudeDeg, radiusM) {
  const earthM = 6371000;
  const lat = (latitudeDeg * Math.PI) / 180;
  const ring = [];
  for (let n = 0; n <= 64; n += 1) {
    const angle = ((n % 64) / 64) * 2 * Math.PI;
    const north = (radiusM * Math.cos(angle)) / earthM;
    const east = (radiusM * Math.sin(angle)) / (earthM * Math.cos(lat));
    ring.push([longitudeDeg + (east * 180) / Math.PI, latitudeDeg + (north * 180) / Math.PI]);
  }
  return { id, geometry: { type: "Polygon", coordinates: [ring] } };
}

/** The latest item of a key in a batch, for a vehicle. */
export function latest(batch, vehicleId, key) {
  let found;
  for (const item of batch.items) {
    if (item.vehicleId === vehicleId && item.key === key) found = item;
  }
  return found;
}
