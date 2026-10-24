import type { CommandCall, Description, Effects, StartInput, TickInput } from "@uxduck/sdk";
import { value } from "@uxduck/sdk";

let startedAt = 0;

export function describe(): Description {
  return {
    panels: [
      {
        id: "timer",
        title: "Session timer",
        tab: true,
        widgets: [
          { type: "display", item: "session.elapsed_s", label: "Elapsed" },
          { type: "button", label: "Reset", command: "reset" },
        ],
      },
    ],
  };
}

function show(nowMs: number): Effects {
  return {
    items: [
      {
        ...value(
          null,
          "session.elapsed_s",
          "Elapsed",
          "second",
          Math.max(0, (nowMs - startedAt) / 1000),
        ),
        ratePerS: 1,
      },
    ],
    store: { startedAt },
    wakeInMs: 60000,
  };
}

export function onStart(input: StartInput): Effects {
  const saved = input.stored.startedAt;
  startedAt = typeof saved === "number" && Number.isFinite(saved) ? saved : input.nowMs;
  return show(input.nowMs);
}

export function onTick(input: TickInput): Effects {
  return show(input.nowMs);
}

export function onCommand(call: CommandCall): Effects {
  if (call.command !== "reset") return {};
  startedAt = Date.now();
  return show(startedAt);
}
