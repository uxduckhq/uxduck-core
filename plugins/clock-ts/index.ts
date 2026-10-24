// A clock, a stopwatch, a countdown timer and an alarm (S101, S104): one
// tab with a switch between them, each a large readout. UXDuck ships this
// plugin.
//
// It keeps its state here in UXDuck, so every window shows the same and a
// timer goes off with the page closed, and stores it (S104), so a running
// stopwatch or timer carries on after UXDuck restarts. It does not tick:
// each running value carries its rate and the app counts on, so it sends
// only when something changes and asks to be woken when the timer ends, or
// in a minute to set the clock again. The time is a moment, not words: the
// app shows it in the time zone of whoever looks, with UTC under it.

import type {
  CommandCall,
  Description,
  Effects,
  ProducedItem,
  StartInput,
  TickInput,
} from "@uxduck/sdk";
import { notify, table, text, value } from "@uxduck/sdk";

const MINUTE_MS = 60_000;

/** The stopwatch: time banked before the current run, and when that run began. */
let stopwatch = { bankedMs: 0, since: null as number | null, laps: [] as number[] };

/** The timer: when it ends, or what was left when paused; what it was set to. */
let timer = {
  endsAt: null as number | null,
  pausedLeftMs: null as number | null,
  setMs: 0,
  done: false,
};

/**
 * The alarm: the moment it rings, in milliseconds since 1970. The app picks
 * the moment from a time of day in the viewer's own zone, so this needs none.
 */
let alarm = { at: null as number | null, ringing: false };

/** How often the clock is set again while nothing else wakes the plugin. */
const RESYNC_MS = MINUTE_MS;

const two = (n: number) => String(n).padStart(2, "0");

/** `1:23.4`: a lap, to the tenth. */
function lapOf(ms: number): string {
  const tenths = Math.floor(ms / 100);
  const s = Math.floor(tenths / 10);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  const rest = `${two(s % 60)}.${tenths % 10}`;
  return h > 0 ? `${h}:${two(m % 60)}:${rest}` : `${m}:${rest}`;
}

function elapsedMs(now: number): number {
  return stopwatch.bankedMs + (stopwatch.since === null ? 0 : now - stopwatch.since);
}

function leftMs(now: number): number {
  if (timer.pausedLeftMs !== null) return timer.pausedLeftMs;
  return timer.endsAt === null ? 0 : Math.max(0, timer.endsAt - now);
}

const alarmSet = () => alarm.at !== null && !alarm.ringing;

function alarmState(): string {
  if (alarm.ringing) return "Ringing";
  return alarm.at === null ? "Not set" : "Counting down";
}

const timerRunning = () => timer.endsAt !== null && timer.pausedLeftMs === null && !timer.done;

function timerState(): string {
  if (timer.done) return "Time is up";
  if (timer.pausedLeftMs !== null) return "Paused";
  return timer.endsAt === null ? "Not set" : "Running";
}

/** A number with the rate it changes at, for the app to count on. */
function counting(key: string, label: string, seconds: number, rate: number): ProducedItem {
  const item = value(null, key, label, key.endsWith("_at") ? "time" : "second", seconds);
  return rate === 0 ? item : { ...item, ratePerS: rate };
}

/** Everything the tab shows, now. UXDuck sends on only what changed. */
function shown(now: number): ProducedItem[] {
  const laps = stopwatch.laps.map((total, n) => [
    String(n + 1),
    lapOf(total - (stopwatch.laps[n - 1] ?? 0)),
    lapOf(total),
  ]);
  return [
    counting("clock.now_at", "Time", now / 1000, 1),
    counting(
      "stopwatch.elapsed_s",
      "Stopwatch",
      elapsedMs(now) / 1000,
      stopwatch.since === null ? 0 : 1,
    ),
    text(null, "stopwatch.state", "Stopwatch", stopwatch.since === null ? "Stopped" : "Running"),
    table(null, "stopwatch.laps", "Laps", laps.reverse()),
    // Still, a timer reads its whole seconds up, as it did while counting.
    timerRunning()
      ? counting("timer.left_s", "Timer", leftMs(now) / 1000, -1)
      : counting("timer.left_s", "Timer", Math.ceil(leftMs(now) / 1000), 0),
    text(null, "timer.state", "Timer", timerState()),
    alarmSet() && alarm.at !== null
      ? counting("alarm.left_s", "Alarm", Math.max(0, alarm.at - now) / 1000, -1)
      : counting("alarm.left_s", "Alarm", 0, 0),
    alarm.at === null
      ? { ...text(null, "alarm.at_at", "Rings at", "—"), unit: "time" }
      : counting("alarm.at_at", "Rings at", alarm.at / 1000, 0),
    text(null, "alarm.state", "Alarm", alarmState()),
  ];
}

/** What to keep, so a restart of UXDuck carries on. */
const kept = (): Effects["store"] => ({ stopwatch, timer, alarm });

/** When to be woken: the timer's end, the alarm, or a minute to set the clock again. */
function wakeIn(now: number): number {
  const ends = [RESYNC_MS];
  if (timerRunning() && timer.endsAt !== null) ends.push(timer.endsAt - now);
  if (alarmSet() && alarm.at !== null) ends.push(alarm.at - now);
  return Math.max(100, Math.min(...ends));
}

/** Tells the plugin's tab, keeps the state, and asks to be woken. */
function changed(now: number, more: Effects = {}): Effects {
  return { ...more, items: shown(now), store: kept(), wakeInMs: wakeIn(now) };
}

export function describe(): Description {
  return {
    panels: [
      {
        id: "clock",
        title: "Clock",
        tab: true,
        widgets: [
          {
            type: "display",
            item: "clock.now_at",
            label: "Time",
            help: "The time where you are, with the date, your time zone and UTC (as flight logs, NOTAMs and ATC use it) under it.",
          },
        ],
      },
      {
        id: "stopwatch",
        title: "Stopwatch",
        tab: true,
        widgets: [
          { type: "display", item: "stopwatch.elapsed_s", label: "Elapsed" },
          { type: "value", item: "stopwatch.state", label: "Status", tone: "muted" },
          { type: "button", label: "Start", command: "stopwatch-start" },
          { type: "button", label: "Stop", command: "stopwatch-stop" },
          { type: "button", label: "Lap", command: "stopwatch-lap" },
          { type: "button", label: "Reset", command: "stopwatch-reset" },
          {
            type: "table",
            item: "stopwatch.laps",
            label: "Laps",
            columns: ["Lap", "Time", "Total"],
            help: "The newest first. It keeps running in UXDuck with the page closed, and after UXDuck restarts.",
          },
        ],
      },
      {
        id: "timer",
        title: "Timer",
        tab: true,
        widgets: [
          { type: "display", item: "timer.left_s", label: "Left" },
          { type: "value", item: "timer.state", label: "Status", tone: "muted" },
          { type: "button", label: "1 min", command: "timer-start", params: { minutes: 1 } },
          { type: "button", label: "5 min", command: "timer-start", params: { minutes: 5 } },
          { type: "button", label: "10 min", command: "timer-start", params: { minutes: 10 } },
          { type: "button", label: "20 min", command: "timer-start", params: { minutes: 20 } },
          { type: "button", label: "+1 min", command: "timer-add", params: { minutes: 1 } },
          { type: "button", label: "Pause", command: "timer-pause" },
          { type: "button", label: "Resume", command: "timer-resume" },
          { type: "button", label: "Cancel", command: "timer-cancel" },
          {
            type: "text",
            text: "When the time is up, UXDuck says so with a warning, wherever you are in it.",
            tone: "muted",
          },
        ],
      },
      {
        id: "alarm",
        title: "Alarm",
        tab: true,
        widgets: [
          { type: "display", item: "alarm.left_s", label: "Rings in" },
          { type: "value", item: "alarm.at_at", label: "Rings at" },
          { type: "value", item: "alarm.state", label: "Status", tone: "muted" },
          {
            type: "time-input",
            label: "Time",
            command: "alarm-set",
            help: "The next time your clock reads this: today, or tomorrow if it has passed. Setting it again replaces the alarm.",
          },
          {
            type: "button",
            label: "Snooze 5 min",
            command: "alarm-snooze",
            params: { minutes: 5 },
          },
          { type: "button", label: "Cancel", command: "alarm-cancel" },
          {
            type: "text",
            text: "When it rings, UXDuck says so with a warning, wherever you are in it.",
            tone: "muted",
          },
        ],
      },
    ],
  };
}

/** The alarm rings: it stays at "Ringing" until set again, snoozed or cancelled. */
function ring(now: number, closed: boolean): Effects {
  alarm.ringing = true;
  return changed(now, {
    notifications: [
      notify(
        null,
        "warning",
        "Alarm",
        closed ? "The alarm rang while UXDuck was closed." : "The alarm is ringing.",
      ),
    ],
  });
}

const alarmDue = (now: number) => alarmSet() && alarm.at !== null && now >= alarm.at;

const minutesText = (ms: number) => `${Math.round((ms / MINUTE_MS) * 10) / 10} min`;

/** Back from what it kept: a timer that ended while UXDuck was closed says so. */
export function onStart(input: StartInput): Effects {
  const stored = input.stored as {
    stopwatch?: typeof stopwatch;
    timer?: typeof timer;
    alarm?: typeof alarm;
  };
  if (stored.stopwatch) stopwatch = { ...stopwatch, ...stored.stopwatch };
  if (stored.timer) timer = { ...timer, ...stored.timer };
  if (stored.alarm) alarm = { ...alarm, ...stored.alarm };
  const now = input.nowMs;
  if (alarmDue(now)) return ring(now, true);
  if (timerRunning() && timer.endsAt !== null && now >= timer.endsAt) {
    timer.endsAt = null;
    timer.done = true;
    return changed(now, {
      notifications: [
        notify(
          null,
          "warning",
          "Time is up",
          `The ${minutesText(timer.setMs)} timer ended while UXDuck was closed.`,
        ),
      ],
    });
  }
  return changed(now);
}

export function onTick(input: TickInput): Effects {
  const now = input.nowMs;
  if (alarmDue(now)) return ring(now, false);
  if (timerRunning() && timer.endsAt !== null && now >= timer.endsAt) {
    timer.endsAt = null;
    timer.done = true;
    return changed(now, {
      notifications: [
        notify(null, "warning", "Time is up", `The ${minutesText(timer.setMs)} timer ended.`),
      ],
    });
  }
  // Set the clock again; the app counts on in between.
  return { items: shown(now), wakeInMs: wakeIn(now) };
}

const minutesOf = (params: unknown, otherwise: number) => {
  const minutes = Number((params as { minutes?: unknown } | null)?.minutes ?? otherwise);
  return Number.isFinite(minutes) ? Math.min(Math.max(minutes, 0.1), 1440) : otherwise;
};

export function onCommand(call: CommandCall): Effects {
  const now = Date.now();
  switch (call.command) {
    case "stopwatch-start":
      if (stopwatch.since === null) stopwatch.since = now;
      break;
    case "stopwatch-stop":
      stopwatch.bankedMs = elapsedMs(now);
      stopwatch.since = null;
      break;
    case "stopwatch-lap":
      if (stopwatch.since !== null) stopwatch.laps.push(elapsedMs(now));
      break;
    case "stopwatch-reset":
      stopwatch = { bankedMs: 0, since: null, laps: [] };
      break;
    case "timer-start":
      timer.setMs = minutesOf(call.params, 5) * MINUTE_MS;
      timer.endsAt = now + timer.setMs;
      timer.pausedLeftMs = null;
      timer.done = false;
      break;
    case "timer-add": {
      const add = minutesOf(call.params, 1) * MINUTE_MS;
      const set = timer.endsAt !== null || timer.pausedLeftMs !== null;
      timer.setMs = set ? timer.setMs + add : add;
      timer.done = false;
      if (timer.pausedLeftMs !== null) timer.pausedLeftMs += add;
      else timer.endsAt = (timer.endsAt ?? now) + add;
      break;
    }
    case "timer-pause":
      if (timerRunning()) timer.pausedLeftMs = leftMs(now);
      break;
    case "timer-resume":
      if (timer.pausedLeftMs !== null) {
        timer.endsAt = now + timer.pausedLeftMs;
        timer.pausedLeftMs = null;
      }
      break;
    case "timer-cancel":
      timer = { endsAt: null, pausedLeftMs: null, setMs: 0, done: false };
      break;
    case "alarm-set": {
      const at = Number((call.params as { at?: unknown } | null)?.at);
      if (!Number.isFinite(at) || at * 1000 <= now || at * 1000 - now > 7 * 24 * 60 * MINUTE_MS) {
        return { log: ["alarm-set needs a moment to come, within a week, in seconds since 1970"] };
      }
      alarm = { at: at * 1000, ringing: false };
      break;
    }
    case "alarm-snooze":
      alarm = { at: now + minutesOf(call.params, 5) * MINUTE_MS, ringing: false };
      break;
    case "alarm-cancel":
      alarm = { at: null, ringing: false };
      break;
    default:
      return { log: [`unknown command ${call.command}`] };
  }
  // Shown at once, not at the next tick, and kept.
  return changed(now);
}
