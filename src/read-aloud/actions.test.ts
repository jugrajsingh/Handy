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
    readClipboard: async () => {
      calls.push("readClipboard");
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
const beforeClipboard = calls.length;
await actions.readClipboard();
assert.deepEqual(
  calls.slice(beforeClipboard),
  ["readClipboard"],
  "Read clipboard calls its command once, in any state",
);
await actions.close();
await actions.readClipboard();
assert.equal(
  calls[calls.length - 1],
  "close",
  "a closed popup never reads the clipboard",
);
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
      readClipboard: async () => {},
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
    readClipboard: async () => {},
    error: () => {},
  },
);
await empty.play();
await empty.stop();
assert.equal(emptyPlays, 0, "empty play dependency count");
assert.equal(idleStops, 0, "idle stop dependency count");
console.log("popup per-state play/stop dependency counts passed");

const { routePopupKey } = await import("./actions");
const keys: string[] = [];
const keyActions = {
  close: () => {
    keys.push("close");
  },
  toggle: () => {
    keys.push("toggle");
  },
};
for (const [key, ctrlKey] of [
  [" ", false],
  ["Enter", false],
  ["Enter", true],
  ["Escape", false],
] as const) {
  routePopupKey(
    {
      key,
      ctrlKey,
      defaultPrevented: false,
      preventDefault: () => {
        keys.push("prevent");
      },
    },
    keyActions,
  );
}
assert.deepEqual(
  keys,
  ["prevent", "toggle", "prevent", "close"],
  "Textarea Space and Enter stay text; Ctrl+Enter and Escape route",
);

{
  const trace: string[] = [];
  let finishReplace: () => void = () => {};
  const view: PopupSnapshot = {
    ...snapshot,
    text: "typed",
    state: "finished",
  };
  const reader = createPopupActions(
    () => view,
    () => {},
    {
      play: async () => {},
      pause: async () => {},
      stop: async () => {
        trace.push("stop");
      },
      replaceText: async (text) => {
        await new Promise<void>((resolve) => {
          finishReplace = resolve;
        });
        trace.push(`replace:${text}`);
      },
      close: async () => {},
      readClipboard: async () => {
        trace.push("readClipboard");
      },
      error: (key) => {
        trace.push(key);
      },
    },
  );
  const editing = reader.edit("typed more");
  const reading = reader.readClipboard();
  await Promise.resolve();
  assert.equal(
    trace.includes("readClipboard"),
    false,
    "Read clipboard waits for the edit in flight",
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  finishReplace();
  await Promise.all([editing, reading]);
  assert.deepEqual(trace, ["stop", "replace:typed more", "readClipboard"]);
  let reported = "";
  const failing = createPopupActions(
    () => view,
    () => {},
    {
      play: async () => {},
      pause: async () => {},
      stop: async () => {},
      replaceText: async () => {},
      close: async () => {},
      readClipboard: async () => {
        throw new Error("clipboard command failed");
      },
      error: (key) => {
        reported = key;
      },
    },
  );
  await failing.readClipboard();
  assert.equal(reported, "tts.errors.generic");
  console.log(
    "popup Read clipboard: command, ordering and error assertions passed",
  );
}
