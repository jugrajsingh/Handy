import assert from "node:assert/strict";
import { createRequire } from "node:module";
import type { AppSettings, HistoryCompareView, Result } from "@/bindings";
import { MAX_DIFF_TOKENS, wordDiff } from "./wordDiff";

assert.deepEqual(wordDiff("hello world", "hello world"), {
  kind: "diff",
  tokens: [
    { kind: "equal", text: "hello" },
    { kind: "equal", text: "world" },
  ],
});
assert.deepEqual(wordDiff("hello", "hello friend"), {
  kind: "diff",
  tokens: [
    { kind: "equal", text: "hello" },
    { kind: "added", text: "friend" },
  ],
});
assert.deepEqual(wordDiff("hello friend", "hello"), {
  kind: "diff",
  tokens: [
    { kind: "equal", text: "hello" },
    { kind: "removed", text: "friend" },
  ],
});
assert.deepEqual(wordDiff("hello, café", "hello! café"), {
  kind: "diff",
  tokens: [
    { kind: "removed", text: "hello," },
    { kind: "added", text: "hello!" },
    { kind: "equal", text: "café" },
  ],
});
assert.deepEqual(
  wordDiff("\n hello\tworld ", "hello world"),
  wordDiff("hello world", "hello world"),
);
assert.deepEqual(wordDiff("a b c", "b c a"), {
  kind: "diff",
  tokens: [
    { kind: "removed", text: "a" },
    { kind: "equal", text: "b" },
    { kind: "equal", text: "c" },
    { kind: "added", text: "a" },
  ],
});
assert.deepEqual(wordDiff("a b a", "a a b"), {
  kind: "diff",
  tokens: [
    { kind: "equal", text: "a" },
    { kind: "removed", text: "b" },
    { kind: "equal", text: "a" },
    { kind: "added", text: "b" },
  ],
});
assert.deepEqual(wordDiff("", ""), { kind: "diff", tokens: [] });
const atCap = Array(MAX_DIFF_TOKENS).fill("a").join(" ");
const aboveCap = `${atCap} a`;
assert.equal(wordDiff(atCap, "").kind, "diff");
assert.equal(wordDiff("", atCap).kind, "diff");
assert.equal(wordDiff(atCap, atCap).kind, "diff");
assert.equal(wordDiff(aboveCap, "").kind, "side_by_side");
assert.equal(wordDiff("", aboveCap).kind, "side_by_side");
console.log("wordDiff: all assertions passed");

const {
  mock,
}: { mock: { module: (name: string, factory: () => unknown) => void } } =
  createRequire(import.meta.url)("bun:test");
const calls: string[] = [];
let saveResponse: () => Promise<Result<null, string>>;
let refreshResponse: () => Promise<Result<AppSettings, string>>;
mock.module("@/bindings", () => ({
  commands: {
    changeHistoryCompareViewSetting: (view: HistoryCompareView) => {
      calls.push(`save:${view}`);
      return saveResponse();
    },
    getAppSettings: () => {
      calls.push("refresh");
      return refreshResponse();
    },
  },
}));
mock.module("@/i18n", () => ({ default: { t: (key: string) => key } }));
mock.module("sonner", () => ({ toast: { error: () => undefined } }));
mock.module("@tauri-apps/api/event", () => ({
  listen: () => {
    throw new Error("Unexpected subscription in compare setting test");
  },
}));
const { useSettingsStore } = await import("../../stores/settingsStore");

function setup() {
  calls.length = 0;
  useSettingsStore.setState({
    settings: { history_compare_view: "diff" },
    isUpdating: {},
  });
  saveResponse = async () => ({ status: "ok", data: null });
  refreshResponse = async () => ({
    status: "ok",
    data: { history_compare_view: "stacked" },
  });
  return useSettingsStore.getState();
}

for (const failure of ["result", "transport"] as const) {
  const store = setup();
  saveResponse = async () => {
    if (failure === "transport") throw new Error("Save failed");
    return { status: "error", error: "Save failed" };
  };
  await assert.rejects(
    store.changeHistoryCompareView("stacked"),
    /Save failed/,
  );
  assert.deepEqual(calls, ["save:stacked"]);
  assert.equal(
    useSettingsStore.getState().settings?.history_compare_view,
    "diff",
  );
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("history_compare_view"),
    false,
  );
}

for (const failure of ["result", "transport"] as const) {
  const store = setup();
  refreshResponse = async () => {
    if (failure === "transport") throw new Error("Refresh failed");
    return { status: "error", error: "Refresh failed" };
  };
  await assert.rejects(
    store.changeHistoryCompareView("stacked"),
    /Refresh failed/,
  );
  assert.deepEqual(calls, ["save:stacked", "refresh"]);
  assert.equal(
    useSettingsStore.getState().settings?.history_compare_view,
    "diff",
  );
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("history_compare_view"),
    false,
  );
}

const store = setup();
let finishSave!: (response: Result<null, string>) => void;
saveResponse = () =>
  new Promise((resolve) => {
    finishSave = resolve;
  });
const pending = store.changeHistoryCompareView("stacked");
assert.deepEqual(calls, ["save:stacked"]);
assert.equal(
  useSettingsStore.getState().isUpdatingKey("history_compare_view"),
  true,
);
assert.equal(
  useSettingsStore.getState().settings?.history_compare_view,
  "diff",
);
finishSave({ status: "ok", data: null });
await pending;
assert.deepEqual(calls, ["save:stacked", "refresh"]);
assert.equal(
  useSettingsStore.getState().settings?.history_compare_view,
  "stacked",
);
assert.equal(
  useSettingsStore.getState().isUpdatingKey("history_compare_view"),
  false,
);

setup();
await useSettingsStore
  .getState()
  .updateSetting("history_compare_view", "stacked");
assert.deepEqual(calls, ["save:stacked", "refresh"]);
assert.equal(
  useSettingsStore.getState().settings?.history_compare_view,
  "stacked",
);
console.log("history compare setting: all assertions passed");
