import assert from "node:assert/strict";
import { selectionEntries, rowActions, type TtsSelection } from "./actions";
const selected: TtsSelection = {
  model_id: "pocket-en",
  voice: "alba",
  speed: 2,
  pitch: 0,
  output_device: null,
  read_code: false,
};
for (const entry of ["settings", "tray"] as const) {
  const trace: string[] = [];
  const errors: string[] = [];
  const actions = selectionEntries({
    select: async (value) => {
      assert.deepEqual(value, selected);
      trace.push("select");
    },
    refresh: async () => {
      trace.push("refresh");
    },
    error: (key) => errors.push(key),
  });
  await actions[entry](selected);
  assert.deepEqual(trace, ["select", "refresh"]);
  assert.deepEqual(errors, []);
}
const errors: string[] = [];
let refreshes = 0;
const failing = selectionEntries({
  select: async () => {
    throw new Error("fixture");
  },
  refresh: async () => {
    refreshes++;
  },
  error: (key) => errors.push(key),
});
await failing.settings(selected);
assert.equal(refreshes, 0);
assert.deepEqual(errors, ["tts.errors.selection"]);
const trace: string[] = [];
const callbacks = {
  use: (id: string) => trace.push(`use:${id}`),
  download: (id: string) => trace.push(`download:${id}`),
  delete: (id: string) => trace.push(`delete:${id}`),
};
const missing = rowActions(
  { id: "pocket-en", downloaded: false },
  { selectedId: "other", downloadId: null, deletingId: null },
  callbacks,
);
assert.equal(missing.canUse, false);
missing.use();
assert.deepEqual(trace, []);
missing.download();
assert.deepEqual(trace, ["download:pocket-en"]);
trace.length = 0;
const busy = rowActions(
  { id: "pocket-en", downloaded: true },
  { selectedId: "other", downloadId: "other", deletingId: null },
  callbacks,
);
busy.use();
busy.delete();
assert.deepEqual(trace, []);
assert.equal(busy.isDownloading, false);
console.log("TTS settings and tray action assertions passed");
