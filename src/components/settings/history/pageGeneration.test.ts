import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { HistoryEntry, PaginatedHistory, Result } from "@/bindings";
import en from "../../../i18n/locales/en/translation.json";
import {
  PageGeneration,
  createHistoryActions,
  createHistoryPageLoader,
  reloadHistoryOnUpdate,
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
let reloads = 0;
for (const action of [
  "added",
  "updated",
  "cleared",
  "deleted",
  "toggled",
] as const) {
  reloadHistoryOnUpdate({ action }, async () => {
    reloads++;
  });
}
assert.equal(reloads, 3);

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
