import assert from "node:assert/strict";
import type { Result } from "@/bindings";
import {
  createLocalLlmActions,
  type LocalLlmActionDependencies,
} from "./localLlmActions";

const ok: Result<null, string> = { status: "ok", data: null };
function setup(overrides: Partial<LocalLlmActionDependencies> = {}) {
  const calls: string[] = [];
  const errors: (string | null)[] = [];
  const deleting: (string | null)[] = [];
  let busy = false;
  const dependencies: LocalLlmActionDependencies = {
    isBusy: () => busy,
    clearError: () => calls.push("clear"),
    setError: (message) => errors.push(message),
    setDeletingId: (id) => {
      deleting.push(id);
      busy = id !== null;
    },
    setLocalLlmModel: async (id) => {
      calls.push(`select:${id}`);
      return ok;
    },
    deleteLocalLlmModel: async (id) => {
      calls.push(`delete:${id}`);
      return ok;
    },
    downloadLocalLlmModel: async (id) => {
      calls.push(`download:${id}`);
    },
    refreshSettingsChecked: async () => {
      calls.push("settings");
    },
    reloadChecked: async () => {
      calls.push("registry");
    },
    modelsChanged: () => calls.push("changed"),
    ...overrides,
  };
  return {
    actions: createLocalLlmActions(dependencies),
    calls,
    errors,
    deleting,
    setBusy: (value: boolean) => {
      busy = value;
    },
  };
}

for (const id of ["downloaded-a", "downloaded-b", "downloaded-c"]) {
  const selection = setup();
  await selection.actions.select(id);
  assert.deepEqual(
    selection.calls,
    ["clear", `select:${id}`, "settings"],
    "selection uses the row ID and awaits checked settings refresh",
  );
  assert.deepEqual(selection.errors, [null]);
  const deletion = setup();
  await deletion.actions.delete(id);
  assert.deepEqual(
    deletion.calls,
    ["clear", `delete:${id}`, "changed", "registry", "settings"],
    "successful deletion invalidates other registry consumers before refreshing",
  );
  assert.deepEqual(deletion.deleting, [id, null]);
  assert.deepEqual(deletion.errors, [null]);
}
const download = setup();
await download.actions.download("missing-a");
await download.actions.download("missing-b");
assert.deepEqual(download.calls, [
  "clear",
  "download:missing-a",
  "clear",
  "download:missing-b",
]);
assert.deepEqual(download.errors, [null, null]);

for (const action of ["select", "delete"] as const) {
  const command =
    action === "select" ? "setLocalLlmModel" : "deleteLocalLlmModel";
  for (const failure of ["result", "transport"] as const) {
    const message = `${action} ${failure} failed`;
    const state = setup({
      [command]: async () => {
        if (failure === "transport") throw new Error(message);
        return { status: "error", error: message };
      },
    });
    await state.actions[action]("downloaded-b");
    assert.deepEqual(
      state.errors,
      [null, message],
      `${action} shows ${failure} failure`,
    );
    assert.deepEqual(
      state.calls,
      ["clear"],
      "failed commands skip invalidation and refresh",
    );
    assert.deepEqual(
      state.deleting,
      action === "delete" ? ["downloaded-b", null] : [],
    );
  }
}
for (const action of ["select", "delete"] as const) {
  const state = setup({
    refreshSettingsChecked: async () => {
      throw new Error("settings refresh failed");
    },
  });
  await state.actions[action]("downloaded-c");
  assert.deepEqual(
    state.errors,
    [null, "settings refresh failed"],
    "checked refresh failure is visible",
  );
  if (action === "delete")
    assert.deepEqual(state.deleting, ["downloaded-c", null]);
}
const reloadFailure = setup({
  reloadChecked: async () => {
    throw new Error("registry reload failed");
  },
});
await reloadFailure.actions.delete("downloaded-a");
assert.deepEqual(reloadFailure.errors, [null, "registry reload failed"]);
assert.deepEqual(reloadFailure.calls, [
  "clear",
  "delete:downloaded-a",
  "changed",
]);
assert.deepEqual(reloadFailure.deleting, ["downloaded-a", null]);

const downloadFailure = setup({
  downloadLocalLlmModel: async () => {
    throw "download transport failed";
  },
});
await downloadFailure.actions.download("missing-a");
assert.deepEqual(downloadFailure.errors, [null, "download transport failed"]);
for (const action of ["select", "delete", "download"] as const) {
  const state = setup();
  state.setBusy(true);
  await state.actions[action]("ignored-id");
  assert.deepEqual(state.calls, [], "busy operations skip IPC");
  assert.deepEqual(
    state.errors,
    [],
    "busy operations retain the current error",
  );
  assert.deepEqual(state.deleting, []);
}

let finishDelete!: (result: Result<null, string>) => void;
const pendingDelete = new Promise<Result<null, string>>((resolve) => {
  finishDelete = resolve;
});
const state = setup({ deleteLocalLlmModel: () => pendingDelete });
const pending = state.actions.delete("downloaded-a");
assert.deepEqual(
  state.deleting,
  ["downloaded-a"],
  "deletion stays busy while IPC is pending",
);
await state.actions.download("missing-a");
await state.actions.select("downloaded-b");
assert.deepEqual(state.calls, ["clear"]);
finishDelete(ok);
await pending;
assert.deepEqual(state.deleting, ["downloaded-a", null]);
assert.deepEqual(state.calls, ["clear", "changed", "registry", "settings"]);

let finishRefresh!: () => void;
const pendingRefresh = new Promise<void>((resolve) => {
  finishRefresh = resolve;
});
const selection = setup({ refreshSettingsChecked: () => pendingRefresh });
let settled = false;
const selecting = selection.actions.select("downloaded-b").then(() => {
  settled = true;
});
await Promise.resolve();
await Promise.resolve();
assert.equal(settled, false, "selection waits for its checked refresh");
finishRefresh();
await selecting;
assert.equal(settled, true);
console.log("localLlmActions: all orchestration assertions passed");
