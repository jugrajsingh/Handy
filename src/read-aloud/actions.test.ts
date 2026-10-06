import assert from "node:assert/strict";
import { controls, createPopupActions, type PopupSnapshot } from "./actions";

let snapshot: PopupSnapshot = {
  epoch: 3,
  state: "waiting",
  text: "",
  source_start: 0,
  source_end: 0,
};
const calls: string[] = [];
let release: () => void = () => {};
const pendingPlay = new Promise<void>((resolve) => {
  release = resolve;
});
const actions = createPopupActions(
  () => snapshot,
  (next) => {
    snapshot = next;
  },
  {
    play: async () => {
      calls.push("play");
      await pendingPlay;
    },
    pause: async () => {
      calls.push("pause");
    },
    stop: async () => {
      calls.push("stop");
    },
    replaceText: async (text) => {
      calls.push(`replace:${text}`);
    },
    close: async () => {
      calls.push("close");
    },
    error: (key) => {
      calls.push(key);
    },
  },
);
assert.equal(controls(snapshot).canPlay, false);
await actions.toggle();
assert.equal(calls.length, 0);
snapshot = { ...snapshot, text: "read this" };
const first = actions.toggle();
await actions.toggle();
assert.deepEqual(calls, ["play"]);
release();
await first;
snapshot = { ...snapshot, state: "playing" };
await actions.toggle();
assert.equal(calls[calls.length - 1], "pause");
await Promise.all([actions.edit("older"), actions.edit("newer")]);
assert.equal(calls.includes("replace:older"), false);
assert.equal(calls[calls.length - 1], "replace:newer");
actions.event({ ...snapshot, epoch: 2, text: "stale" });
assert.equal(snapshot.text, "read this");
await actions.close();
actions.event({ ...snapshot, epoch: 99, text: "late" });
assert.equal(snapshot.text, "read this");
console.log(
  "read-aloud actions: empty/busy/edit/epoch/close assertions passed",
);

for (const [state, playCount, stopCount] of [
  ["starting", 0, 1],
  ["paused", 1, 1],
  ["error", 1, 1],
  ["playing", 1, 1],
  ["waiting", 1, 0],
] as const) {
  let plays = 0;
  let stops = 0;
  const view: PopupSnapshot = { ...snapshot, text: "read this", state };
  const guarded = createPopupActions(
    () => view,
    () => {},
    {
      play: async () => {
        plays++;
      },
      pause: async () => {},
      stop: async () => {
        stops++;
      },
      replaceText: async () => {},
      close: async () => {},
      error: () => {},
    },
  );
  await guarded.play();
  await guarded.stop();
  assert.equal(plays, playCount, `${state} play dependency count`);
  assert.equal(stops, stopCount, `${state} stop dependency count`);
}
let emptyPlays = 0;
let idleStops = 0;
const empty = createPopupActions(
  () => ({ ...snapshot, text: "", state: "waiting" }),
  () => {},
  {
    play: async () => {
      emptyPlays++;
    },
    pause: async () => {},
    stop: async () => {
      idleStops++;
    },
    replaceText: async () => {},
    close: async () => {},
    error: () => {},
  },
);
await empty.play();
await empty.stop();
assert.equal(emptyPlays, 0, "empty play dependency count");
assert.equal(idleStops, 0, "idle stop dependency count");
console.log("popup per-state play/stop dependency counts passed");
