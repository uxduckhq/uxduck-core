import type {
  Description,
  Effects,
  ItemBatch,
  PluginNotification,
  ProducedItem,
} from "@uxduck/sdk";
import { notify, value } from "@uxduck/sdk";

const warned = new Set<string>();

export function describe(): Description {
  return {
    panels: [
      {
        id: "battery",
        title: "Battery watch",
        tab: true,
        vehicle: true,
        widgets: [
          {
            type: "gauge",
            item: "battery_watch.remaining_pct",
            label: "Battery",
            min: 0,
            max: 100,
          },
        ],
      },
    ],
  };
}

export function onItems(batch: ItemBatch): Effects {
  const items: ProducedItem[] = [];
  const notifications: PluginNotification[] = [];
  for (const item of batch.items) {
    if (item.key !== "battery.remaining_pct" || item.value == null) continue;
    items.push(
      value(item.vehicleId, "battery_watch.remaining_pct", "Battery", "percent", item.value),
    );
    if (item.value < 20 && !warned.has(item.vehicleId)) {
      warned.add(item.vehicleId);
      notifications.push(notify(item.vehicleId, "warning", "Battery below 20 percent"));
    } else if (item.value >= 25) {
      warned.delete(item.vehicleId);
    }
  }
  return { items, notifications };
}
