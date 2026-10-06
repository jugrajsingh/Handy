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

let releaseSave!: () => void;
const firstSave = new Promise<void>((resolve) => {
  releaseSave = resolve;
});
const saved: TtsSelection[] = [];
const displayed: TtsSelection[] = [];
const editor = selectionEntries({
  select: async (value) => {
    saved.push(value);
    if (saved.length === 1) await firstSave;
  },
  refresh: async () => {},
  error: () => assert.fail("unexpected save error"),
  display: (value) => displayed.push(value),
});
const firstEdit = editor.edit({ speed: 1.1 }, selected);
void editor.edit({ speed: 1.5, pitch: 2 }, selected);
void editor.edit({ speed: 2, voice: "m2", model_id: "supertonic-3" }, selected);
const finalSelection = {
  ...selected,
  speed: 2,
  pitch: 2,
  voice: "m2",
  model_id: "supertonic-3",
};
assert.deepEqual(
  displayed[displayed.length - 1],
  finalSelection,
  "pending edits display the latest draft",
);
editor.sync({ ...selected, speed: 1.1 });
assert.deepEqual(
  displayed[displayed.length - 1],
  finalSelection,
  "pending refresh cannot snap controls back",
);
releaseSave();
await firstEdit;
assert.deepEqual(
  saved,
  [{ ...selected, speed: 1.1 }, finalSelection],
  "final-save drain persists the latest merged selection",
);
assert.deepEqual(
  displayed[displayed.length - 1],
  finalSelection,
  "final saved selection stays displayed",
);
console.log("deferred selection saves retain and display the final edit");

const { downloadController } = await import("./actions");
let releaseDownload!: () => void;
let rejectDownload!: (reason: Error) => void;
let activeBackend: string[] = ["pocket-en"];
let progressListener!: (value: import("./ModelCard").DownloadProgress) => void;
let downloadCalls = 0;
const cancelled: string[] = [];
const downloads = downloadController({
  query: async () => activeBackend,
  watch: async (listener) => {
    progressListener = listener;
  },
  download: async (id) => {
    downloadCalls++;
    activeBackend = [id];
    await new Promise<void>((resolve, reject) => {
      releaseDownload = resolve;
      rejectDownload = reject;
    });
    activeBackend = [];
  },
  cancel: async (id) => {
    cancelled.push(id);
  },
});
let notifications = 0;
let detach = downloads.subscribe(() => notifications++);
await downloads.refresh();
const downloadRow = () =>
  rowActions(
    { id: "pocket-en", downloaded: false },
    {
      selectedId: "other",
      downloadId: downloads.snapshot().active[0] ?? null,
      deletingId: null,
    },
    {
      use: () => {},
      delete: () => {},
      download: (id) => void downloads.download(id),
    },
  );
assert.equal(
  downloadRow().isDownloading,
  true,
  "mount queries backend download ownership",
);
assert.equal(downloadRow().canDownload, false);
progressListener({
  model_id: "pocket-en",
  downloaded_bytes: 7,
  total_bytes: 100,
});
await downloads.cancel("pocket-en");
assert.deepEqual(cancelled, ["pocket-en"]);
assert.equal(downloadCalls, 0, "remount never replaces the backend job");
activeBackend = [];
await downloads.refresh();
assert.deepEqual(
  downloads.snapshot().progress,
  {},
  "cancelled backend jobs clear progress",
);
progressListener({
  model_id: "pocket-en",
  downloaded_bytes: 8,
  total_bytes: 100,
});
assert.equal(
  downloads.snapshot().active.length,
  0,
  "late progress cannot revive a cancelled backend job",
);
const job = downloads.download("pocket-en");
progressListener({
  model_id: "pocket-en",
  downloaded_bytes: 42,
  total_bytes: 100,
});
detach();
assert.equal(
  downloadRow().isDownloading,
  true,
  "detach preserves shared download ownership",
);
detach = downloads.subscribe(() => notifications++);
assert.equal(
  downloadRow().isDownloading,
  true,
  "reattach retains Cancel before its backend query settles",
);
assert.equal(
  downloads.snapshot().progress["pocket-en"].downloaded_bytes,
  42,
  "navigation retains progress before any new event",
);
await downloads.refresh();
assert.equal(
  downloadRow().isDownloading,
  true,
  "remount shows Cancel for the active job",
);
assert.equal(
  downloadRow().canDownload,
  false,
  "remount disables duplicate Download",
);
assert.equal(downloads.snapshot().progress["pocket-en"].downloaded_bytes, 42);
detach();
const beforeCompletion = notifications;
releaseDownload();
await job;
assert.equal(
  notifications,
  beforeCompletion,
  "detached subscribers receive no completion updates",
);
detach = downloads.subscribe(() => notifications++);
await downloads.refresh();
assert.equal(downloadRow().isDownloading, false);
assert.equal(downloadRow().canDownload, true);
assert.deepEqual(
  downloads.snapshot().progress,
  {},
  "completed jobs clear progress",
);
progressListener({
  model_id: "pocket-en",
  downloaded_bytes: 100,
  total_bytes: 100,
});
assert.equal(
  downloads.snapshot().active.length,
  0,
  "late progress cannot revive a completed job",
);
const failedJob = downloads.download("pocket-en");
progressListener({
  model_id: "pocket-en",
  downloaded_bytes: 10,
  total_bytes: 100,
});
detach();
activeBackend = [];
rejectDownload(new Error("tts.errors.download"));
await failedJob;
detach = downloads.subscribe(() => notifications++);
await downloads.refresh();
assert.equal(
  downloads.snapshot().error,
  "tts.errors.download",
  "download errors survive navigation",
);
assert.deepEqual(
  downloads.snapshot().progress,
  {},
  "failed jobs clear progress",
);
detach();
console.log(
  "backend download ownership, navigation, cancellation and completion assertions passed",
);

let releaseFailedSave!: () => void;
const failedSave = new Promise<void>((resolve) => {
  releaseFailedSave = resolve;
});
const afterFailure: TtsSelection[] = [];
const saveErrors: string[] = [];
const recoveringEditor = selectionEntries({
  select: async (value) => {
    afterFailure.push(value);
    if (afterFailure.length === 1) {
      await failedSave;
      throw new Error("fixture");
    }
  },
  refresh: async () => {},
  error: (key) => saveErrors.push(key),
});
const recovery = recoveringEditor.edit({ speed: 1.1 }, selected);
void recoveringEditor.edit(
  { pitch: 4, output_device: "fixture", read_code: true },
  selected,
);
releaseFailedSave();
await recovery;
assert.equal(afterFailure.length, 2, "failed saves still drain later edits");
assert.deepEqual(afterFailure[1], {
  ...selected,
  speed: 1.1,
  pitch: 4,
  output_device: "fixture",
  read_code: true,
});
assert.deepEqual(saveErrors, ["tts.errors.selection"]);
