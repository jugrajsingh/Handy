import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { HistoryEntry, PaginatedHistory, Result } from "@/bindings";
import en from "../../../i18n/locales/en/translation.json";
import {
  PageGeneration,
  createHistoryActions,
  createHistoryPageLoader,
  reloadHistoryOnUpdate,
  mergeHistoryEntries,
  historyAnchorScrollTop,
  createHistoryScrollAnchor,
  createHistoryEntryActions,
  retryHistoryWithAnchor,
} from "./pageGeneration";
const gate = new PageGeneration();
const beforeClear = gate.current();
const clearReload = gate.beginReset();
assert.equal(gate.isCurrent(beforeClear), false);
assert.equal(gate.isCurrent(clearReload), true);
const nextReload = gate.beginReset();
assert.equal(gate.isCurrent(clearReload), false);
assert.equal(gate.isCurrent(nextReload), true);

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
const row = (id: number): HistoryEntry => ({
  id,
  file_name: `${id}.wav`,
  timestamp: id,
  saved: id === 2,
  title: "",
  transcription_text: `row ${id}`,
  post_processed_text: null,
  post_process_prompt: null,
  post_process_requested: false,
  post_process_provider: null,
  post_process_model: null,
});
const page = (...ids: number[]): Result<PaginatedHistory, string> => ({
  status: "ok",
  data: { entries: ids.map(row), has_more: ids.length > 0 },
});
const requests: ReturnType<
  typeof deferred<Result<PaginatedHistory, string>>
>[] = [];
const cursors: (number | null)[] = [];
let entries: HistoryEntry[] = [];
const getEntries = (): HistoryEntry[] => entries;
let loading = false;
let hasMore = true;
let error: string | null = null;
const loadingRef = { current: false };
const loader = createHistoryPageLoader({
  getEntries,
  generation: new PageGeneration(),
  loadingRef,
  fetchPage: (cursor, limit) => {
    assert.equal(limit, 30);
    cursors.push(cursor);
    const request = deferred<Result<PaginatedHistory, string>>();
    requests.push(request);
    return request.promise;
  },
  setEntries: (update) => {
    entries = update(entries);
  },
  setLoading: (value) => {
    loading = value;
  },
  setHasMore: (value) => {
    hasMore = value;
  },
  setError: (value) => {
    error = value;
  },
});
const staleInitial = loader.loadPage();
const afterClear = loader.loadPage();
requests[0].resolve(page(1));
await staleInitial;
assert.deepEqual(getEntries(), []);
assert.equal(loading, true);
assert.equal(loadingRef.current, true);
requests[1].resolve(page(2));
await afterClear;
assert.deepEqual(
  getEntries().map((entry) => entry.id),
  [2],
);
assert.equal(loading, false);
const stalePagination = loader.loadPage(2);
await loader.loadPage(2);
assert.equal(requests.length, 3);
const cleared = loader.loadPage();
requests[3].resolve(page());
await cleared;
requests[2].resolve(page(1));
await stalePagination;
assert.deepEqual(getEntries(), []);
assert.equal(hasMore, false);
const clearedSnapshot = loader.loadPage();
const postClearSave = loader.loadPage();
requests[5].resolve(page(3, 2));
await postClearSave;
requests[4].resolve(page(2));
await clearedSnapshot;
assert.deepEqual(
  getEntries().map((entry) => entry.id),
  [3, 2],
);
const appended = loader.loadPage(2);
requests[6].resolve(page(4));
await appended;
assert.deepEqual(
  getEntries().map((entry) => entry.id),
  [3, 2, 4],
);
const staleError = loader.loadPage();
const current = loader.loadPage();
requests[7].reject(new Error("old failure"));
await staleError;
assert.equal(error, null);
assert.equal(loadingRef.current, true);
requests[8].resolve(page(3));
await current;
const failure = loader.loadPageChecked();
requests[9].resolve({ status: "error", error: "page failed" });
await assert.rejects(failure, /page failed/);
assert.equal(error, "page failed");
assert.equal(loading, false);
assert.equal(loadingRef.current, false);
const transportFailure = loader.loadPageChecked();
requests[10].reject(new Error("transport failed"));
await assert.rejects(transportFailure, /transport failed/);
assert.equal(error, "transport failed");
assert.deepEqual(cursors.slice(0, 4), [null, null, 2, null]);
const loaded = [row(90), row(80), row(70)];
const changed = { ...row(80), transcription_text: "new text" };
assert.deepEqual(
  mergeHistoryEntries(loaded, { action: "updated", entry: changed }),
  [loaded[0], changed, loaded[2]],
);
assert.equal(
  mergeHistoryEntries(loaded, { action: "updated", entry: row(60) }),
  loaded,
);
assert.deepEqual(
  mergeHistoryEntries(loaded, { action: "added", entry: row(100) }).map(
    (e) => e.id,
  ),
  [100, 90, 80, 70],
);
assert.deepEqual(
  mergeHistoryEntries(loaded, { action: "added", entry: changed }).map(
    (e) => e.id,
  ),
  [90, 80, 70],
);
assert.equal(
  mergeHistoryEntries(loaded, { action: "deleted", id: 80 }),
  loaded,
);
assert.equal(historyAnchorScrollTop(20, 100, 900), 980);
assert.equal(historyAnchorScrollTop(100, 20, 900), 820);
assert.equal(historyAnchorScrollTop(100, 20, 10), 0);
let reloads = 0;
let merges = 0;
for (const payload of [
  { action: "added", entry: row(100) },
  { action: "updated", entry: row(80) },
  { action: "cleared" },
  { action: "deleted", id: 80 },
  { action: "toggled", id: 80 },
] as const) {
  reloadHistoryOnUpdate(
    payload,
    async () => {
      reloads++;
    },
    () => {
      merges++;
    },
  );
}
assert.equal(reloads, 1);
assert.equal(merges, 2);
console.log("history event merge and anchor: all assertions passed");

const fetches: ReturnType<typeof deferred<Result<PaginatedHistory, string>>>[] =
  [];
const seenCursors: (number | null)[] = [];
let visible: HistoryEntry[] = [];
let more = true;
let commits = 0;
const eventGeneration = new PageGeneration();
const eventLoader = createHistoryPageLoader({
  getEntries: () => visible,
  generation: eventGeneration,
  loadingRef: { current: false },
  fetchPage: (cursor) => {
    seenCursors.push(cursor);
    const request = deferred<Result<PaginatedHistory, string>>();
    fetches.push(request);
    return request.promise;
  },
  beforeCommit: () => {
    commits++;
  },
  setEntries: (update) => {
    visible = update(visible);
  },
  setLoading: () => undefined,
  setHasMore: (value) => {
    more = value;
  },
  setError: (value) => {
    assert.equal(value, null);
  },
});
const initial = eventLoader.loadPage();
eventLoader.applyHistoryUpdate({ action: "added", entry: row(100) });
fetches[0].resolve(page(90, 80));
await initial;
assert.deepEqual(
  visible.map((e) => e.id),
  [100, 90, 80],
);
const revision = eventGeneration.current();
const older = eventLoader.loadPage(80);
eventLoader.applyHistoryUpdate({ action: "updated", entry: changed });
eventLoader.applyHistoryUpdate({
  action: "updated",
  entry: { ...row(70), transcription_text: "fresh 70" },
});
eventLoader.applyHistoryUpdate({ action: "added", entry: row(110) });
fetches[1].resolve(page(80, 70, 60));
await older;
assert.deepEqual(
  visible.map((e) => e.id),
  [110, 100, 90, 80, 70, 60],
);
assert.equal(visible.find((e) => e.id === 80)?.transcription_text, "new text");
assert.equal(visible.find((e) => e.id === 70)?.transcription_text, "fresh 70");
eventLoader.applyHistoryUpdate({ action: "updated", entry: row(10) });
assert.equal(
  visible.some((e) => e.id === 10),
  false,
);
assert.equal(visible[visible.length - 1]?.id, 60);
assert.equal(eventGeneration.current(), revision);
assert.equal(more, true);
assert.deepEqual(seenCursors, [null, 80]);
assert.equal(commits, 5);
console.log("history fetch overlays: all assertions passed");

let entryActionError: string | null = null;
let actionMode: "ok" | "error" | "throw" = "ok";
let pendingToggle: ReturnType<typeof deferred<Result<null, string>>> | null =
  null;
let entryReloads = 0;
const entryActions = createHistoryEntryActions({
  getEntry: (id) => visible.find((e) => e.id === id),
  setSaved: eventLoader.setSaved,
  removeEntry: eventLoader.removeEntry,
  toggleSaved: async () => {
    if (pendingToggle) return pendingToggle.promise;
    if (actionMode === "throw") throw new Error("toggle transport failed");
    return actionMode === "error"
      ? { status: "error", error: "toggle failed" }
      : { status: "ok", data: null };
  },
  deleteEntry: async () => {
    if (actionMode === "throw") throw new Error("delete transport failed");
    return actionMode === "error"
      ? { status: "error", error: "delete failed" }
      : { status: "ok", data: null };
  },
  reload: async () => {
    entryReloads++;
  },
  setError: (value) => {
    entryActionError = value;
  },
});
const deletionFetch = eventLoader.loadPage(60);
await entryActions.deleteEntry(110);
eventLoader.applyHistoryUpdate({ action: "deleted", id: 100 });
eventLoader.applyHistoryUpdate({ action: "updated", entry: row(110) });
fetches[2].resolve(page(110, 100, 60, 50));
await deletionFetch;
assert.deepEqual(
  visible.map((e) => e.id),
  [90, 80, 70, 60, 50],
);
pendingToggle = deferred<Result<null, string>>();
const saveFetch = eventLoader.loadPage(50);
const saving = entryActions.toggleSaved(80);
assert.equal(visible.find((e) => e.id === 80)?.saved, true);
eventLoader.applyHistoryUpdate({
  action: "updated",
  entry: { ...changed, transcription_text: "latest 80" },
});
eventLoader.applyHistoryUpdate({ action: "toggled", id: 80 });
fetches[3].resolve(page(80, 50, 40));
await saveFetch;
assert.equal(visible.find((e) => e.id === 80)?.saved, true);
pendingToggle.resolve({ status: "ok", data: null });
await saving;
pendingToggle = null;
for (const failureMode of ["error", "throw"] as const) {
  actionMode = failureMode;
  await entryActions.toggleSaved(80);
  assert.equal(visible.find((e) => e.id === 80)?.saved, true);
  assert.equal(
    entryActionError,
    failureMode === "error" ? "toggle failed" : "toggle transport failed",
  );
  const rollbackFetch = eventLoader.loadPage(40);
  fetches[fetches.length - 1].resolve(page(80, 40, 30));
  await rollbackFetch;
  assert.equal(visible.find((e) => e.id === 80)?.saved, true);
  assert.equal(
    visible.find((e) => e.id === 80)?.transcription_text,
    "latest 80",
  );
  await entryActions.deleteEntry(30);
  assert.equal(
    entryActionError,
    failureMode === "error" ? "delete failed" : "delete transport failed",
  );
}
assert.equal(entryReloads, 2);
const resetEvents = eventLoader.loadPage();
fetches[fetches.length - 1].resolve(page(110, 80));
await resetEvents;
assert.deepEqual(
  visible.map((e) => e.id),
  [110, 80],
);
assert.equal(visible[1].saved, false);
console.log("history deletion and saved overlays: all assertions passed");

const viewport = {
  top: 0,
  scrollTop: 900,
  rows: [
    { id: "100", top: -100, bottom: -20 },
    { id: "80", top: 20, bottom: 120 },
  ],
};
const anchor = createHistoryScrollAnchor({
  getViewport: () => viewport,
  setScrollTop: (value) => {
    viewport.scrollTop = value;
  },
});
anchor.capture();
viewport.rows[1] = { id: "80", top: 100, bottom: 200 };
anchor.capture();
anchor.restore();
assert.equal(viewport.scrollTop, 980);
// User scrolling between capture and commit preserves the new position.
viewport.rows[1] = { id: "80", top: 20, bottom: 120 };
anchor.capture();
viewport.scrollTop = 1000;
viewport.rows[1] = { id: "80", top: 0, bottom: 100 };
anchor.restore();
assert.equal(viewport.scrollTop, 1000);
// Retry start, fetch commit, and retry completion each compensate their height change.
for (const delta of [-80, 120, 60]) {
  viewport.rows[1] = { id: "80", top: 20, bottom: 120 };
  const currentScrollTop: number = viewport.scrollTop;
  anchor.capture();
  viewport.rows[1] = { id: "80", top: 20 + delta, bottom: 120 + delta };
  anchor.restore();
  assert.equal(viewport.scrollTop, currentScrollTop + delta);
}
anchor.capture();
anchor.clear();
viewport.rows[1].top += 100;
const clearedScrollTop = viewport.scrollTop;
anchor.restore();
assert.equal(viewport.scrollTop, clearedScrollTop);
viewport.rows = [];
anchor.capture();
anchor.restore();
assert.equal(viewport.scrollTop, clearedScrollTop);
const retryOrder: string[] = [];
const retryWait = deferred<void>();
let retryBusy = false;
const retry = retryHistoryWithAnchor(80, {
  captureAnchor: () => {
    retryOrder.push("capture");
    anchor.capture();
  },
  setRetrying: (value) => {
    retryOrder.push(`busy ${value}`);
    retryBusy = value;
    viewport.rows = [{ id: "80", top: value ? 0 : 140, bottom: 200 }];
    anchor.restore();
  },
  retry: async (id) => {
    assert.equal(id, 80);
    retryOrder.push("request");
    await retryWait.promise;
  },
});
assert.equal(retryBusy, true);
anchor.capture();
viewport.rows[0].top += 40;
anchor.restore();
const beforeRetryCompletion = viewport.scrollTop;
retryWait.resolve();
await retry;
assert.equal(retryBusy, false);
assert.equal(viewport.scrollTop, beforeRetryCompletion + 100);
assert.deepEqual(retryOrder, [
  "capture",
  "busy true",
  "request",
  "capture",
  "busy false",
]);
await assert.rejects(
  retryHistoryWithAnchor(80, {
    captureAnchor: () => {
      retryOrder.push("capture");
    },
    setRetrying: (value) => {
      retryBusy = value;
    },
    retry: async () => {
      throw new Error("retry failed");
    },
  }),
  /retry failed/,
);
assert.equal(retryBusy, false);
console.log("history viewport seam: all assertions passed");

let confirmation = false;
let clearCalls = 0;
let actionReloads = 0;
let summaryCalls = 0;
let clearing = false;
let changing = false;
let mode:
  | "ok"
  | "summary"
  | "dialog"
  | "clear"
  | "reload"
  | "command"
  | "refresh" = "ok";
const viewCalls: string[] = [];
const summaryWait =
  deferred<Result<{ entries: number; recordings: number }, string>>();
let waitForSummary = false;
const viewWait = deferred<void>();
let waitForView = false;
const actions = createHistoryActions({
  getSummary: async (keepSaved) => {
    assert.equal(keepSaved, true);
    summaryCalls++;
    if (waitForSummary) return summaryWait.promise;
    return mode === "summary"
      ? { status: "error", error: "summary failed" }
      : { status: "ok", data: { entries: 5, recordings: 3 } };
  },
  confirm: async (summary) => {
    assert.deepEqual(summary, { entries: 5, recordings: 3 });
    assert.equal(clearing, true);
    if (mode === "dialog") throw new Error("dialog failed");
    return confirmation;
  },
  clear: async (keepSaved) => {
    assert.equal(keepSaved, true);
    clearCalls++;
    return mode === "clear"
      ? { status: "error", error: "clear failed" }
      : { status: "ok", data: { entries: 5, recordings: 3 } };
  },
  reload: async () => {
    actionReloads++;
    if (mode === "reload") throw new Error("reload failed");
  },
  changeCompareView: async (view) => {
    viewCalls.push(view);
    assert.equal(changing, true);
    if (waitForView) await viewWait.promise;
    if (mode === "command" || mode === "refresh")
      throw new Error(`${mode} failed`);
  },
  setClearing: (value) => {
    clearing = value;
  },
  setChangingView: (value) => {
    changing = value;
  },
  setError: (value) => {
    error = value;
  },
});
await actions.clearHistory();
assert.equal(clearCalls, 0);
assert.equal(actionReloads, 0);
assert.equal(clearing, false);
confirmation = true;
waitForSummary = true;
const busyClear = actions.clearHistory();
assert.equal(clearing, true);
await actions.clearHistory();
assert.equal(summaryCalls, 2);
assert.equal(clearCalls, 0);
summaryWait.resolve({ status: "ok", data: { entries: 5, recordings: 3 } });
await busyClear;
waitForSummary = false;
assert.equal(clearCalls, 1);
assert.equal(actionReloads, 1);
assert.equal(clearing, false);
for (const failureMode of ["summary", "dialog", "clear", "reload"] as const) {
  mode = failureMode;
  const previousClears: number = clearCalls;
  const previousReloads: number = actionReloads;
  await actions.clearHistory();
  assert.equal(error, `${failureMode} failed`);
  assert.equal(clearing, false);
  assert.equal(
    clearCalls - previousClears,
    failureMode === "clear" || failureMode === "reload" ? 1 : 0,
  );
  assert.equal(
    actionReloads - previousReloads,
    failureMode === "reload" ? 1 : 0,
  );
}
mode = "ok";
await actions.changeView("invalid");
assert.deepEqual(viewCalls, []);
waitForView = true;
const busyView = actions.changeView("stacked");
assert.equal(changing, true);
await actions.changeView("diff");
assert.deepEqual(viewCalls, ["stacked"]);
viewWait.resolve();
await busyView;
assert.equal(changing, false);
waitForView = false;
for (const failureMode of ["command", "refresh"] as const) {
  mode = failureMode;
  await actions.changeView("side_by_side");
  assert.equal(error, `${failureMode} failed`);
  assert.equal(changing, false);
}
mode = "ok";
await actions.changeView("diff");
assert.equal(error, null);

const pageSource = readFileSync(
  new URL("./HistorySettings.tsx", import.meta.url),
  "utf8",
);
const dialogSource = pageSource.match(
  /confirm: \(summary\) =>([\s\S]*?)\n\s*setClearing,/u,
)?.[1];
assert.ok(dialogSource, "Clear History confirmation must be present");
const dialogKeys = Array.from(
  dialogSource.matchAll(/\bt\("([^"]+)"/gu),
  (match) => match[1],
);
assert.equal(
  dialogKeys.length,
  4,
  "Audit the message, title, OK and Cancel translations",
);
function englishValue(key: string): unknown {
  let value: unknown = en;
  for (const part of key.split(".")) {
    if (typeof value !== "object" || value === null || !(part in value))
      return undefined;
    value = (value as Record<string, unknown>)[part];
  }
  return value;
}
for (const key of dialogKeys) {
  assert.equal(
    typeof englishValue(key),
    "string",
    `Missing English dialog translation: ${key}`,
  );
}
const cancelKey = dialogSource.match(/cancelLabel:\s*t\("([^"]+)"\)/u)?.[1];
assert.ok(cancelKey, "Clear History cancel label must use a translation");
assert.equal(englishValue(cancelKey), "Cancel");
console.log("pageGeneration: all assertions passed");
