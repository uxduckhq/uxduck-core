import type { Effects, MapFeature, MessageBatch } from "@uxduck/sdk";
import { point } from "@uxduck/sdk";

export function onMessages(batch: MessageBatch): Effects {
  // Features share one plugin layer: include the vehicle id to avoid collisions.
  const features = new Map<string, MapFeature>();
  for (const message of batch.messages) {
    if (message.name !== "GLOBAL_POSITION_INT") continue;
    const { lat, lon } = message.fields;
    if (typeof lat !== "number" || typeof lon !== "number") continue;
    const id = `vehicle-${message.vehicleId}`;
    features.set(id, point(id, lat / 1e7, lon / 1e7));
  }
  return { features: [...features.values()] };
}
