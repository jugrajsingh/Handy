import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createInstance } from "i18next";
import translations from "../i18n/locales/en/translation.json";
import type {
  AppSettings,
  BindingResponse,
  Result,
  ShortcutBinding,
} from "@/bindings";

// The frontend type dependencies contain Node types, without Bun test types.
const {
  test,
  mock,
}: {
  test: (name: string, run: () => void | Promise<void>) => void;
  mock: { module: (name: string, factory: () => unknown) => void };
} = createRequire(import.meta.url)("bun:test");

const i18n = createInstance();
await i18n.init({
  lng: "en",
  resources: { en: { translation: translations } },
});
const notifications: string[] = [];
const calls: { name: string; args: unknown[] }[] = [];
const rawId = "transcribe";
const cleanId = "transcribe_with_post_process";
const rawDefault = "ctrl+shift+d";
const cleanKeys = "super+alt+d";
const binding = (id: string, keys: string): ShortcutBinding => ({
  id,
  name: id,
  description: id,
  default_binding: rawDefault,
  current_binding: keys,
});
const fixture = (raw = rawDefault, enabled = true): AppSettings => ({
  post_process_enabled: enabled,
  bindings: {
    [rawId]: binding(rawId, raw),
    [cleanId]: binding(cleanId, cleanKeys),
  },
});
let persisted: AppSettings;
let clearResponse: () => Promise<Result<ShortcutBinding, string>>;
let resetResponse: () => Promise<Result<BindingResponse, string>>;
let changeResponse: () => Promise<Result<BindingResponse, string>>;
let enableResponse: () => Promise<Result<null, string>>;

mock.module("@/bindings", () => ({
  commands: {
    clearBinding: (id: string) => {
      calls.push({ name: "clear", args: [id] });
      return clearResponse();
    },
    resetBinding: (id: string) => {
      calls.push({ name: "reset", args: [id] });
      return resetResponse();
    },
    changeBinding: (id: string, keys: string) => {
      calls.push({ name: "change", args: [id, keys] });
      return changeResponse();
    },
    changePostProcessEnabledSetting: (enabled: boolean) => {
      calls.push({ name: "enable", args: [enabled] });
      return enableResponse();
    },
    getAppSettings: async () => {
      calls.push({ name: "refresh", args: [] });
      return { status: "ok", data: persisted };
    },
  },
}));
mock.module("@/i18n", () => ({ default: i18n }));
mock.module("sonner", () => ({
  toast: { error: (message: string) => notifications.push(message) },
}));
mock.module("@tauri-apps/api/event", () => ({
  listen: () => {
    throw new Error("Unexpected event subscription in shortcut test");
  },
}));
const { useSettingsStore } = await import("./settingsStore");

function setup(raw = rawDefault, enabled = true) {
  calls.length = 0;
  notifications.length = 0;
  persisted = fixture(raw, enabled);
  useSettingsStore.setState({
    settings: fixture(raw, enabled),
    isUpdating: {},
    isLoading: false,
  });
  clearResponse = async () => ({ status: "ok", data: binding(rawId, "") });
  resetResponse = async () => ({
    status: "ok",
    data: { success: true, binding: binding(rawId, rawDefault), error: null },
  });
  changeResponse = async () => ({
    status: "ok",
    data: { success: true, binding: null, error: null },
  });
  enableResponse = async () => ({ status: "ok", data: null });
  return useSettingsStore.getState();
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function errorNotification(message: string) {
  return i18n.t("settings.general.shortcut.errors.set", {
    error: `Error: ${message}`,
  });
}

const rawKeys = () =>
  useSettingsStore.getState().settings?.bindings?.[rawId]?.current_binding;

test("Clear success refreshes the real store and clears updating state", async () => {
  const store = setup();
  const response = deferred<Result<ShortcutBinding, string>>();
  clearResponse = () => response.promise;
  const pending = store.clearBinding(rawId);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    true,
  );
  assert.equal(rawKeys(), rawDefault);
  persisted = fixture("");
  response.resolve({ status: "ok", data: binding(rawId, "") });
  await pending;
  assert.equal(rawKeys(), "");
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    false,
  );
  assert.deepEqual(calls, [
    { name: "clear", args: [rawId] },
    { name: "refresh", args: [] },
  ]);
  assert.deepEqual(notifications, []);
});

test("Clear refusal propagates to the input and cleans updating without refresh", async () => {
  const store = setup();
  const response = deferred<Result<ShortcutBinding, string>>();
  clearResponse = () => response.promise;
  const pending = store.clearBinding(rawId);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    true,
  );
  const rejected = assert.rejects(
    pending,
    /At least one dictation shortcut is required/,
  );
  response.resolve({
    status: "error",
    error: "At least one dictation shortcut is required",
  });
  await rejected;
  assert.equal(rawKeys(), rawDefault);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    false,
  );
  assert.deepEqual(calls, [{ name: "clear", args: [rawId] }]);
  assert.deepEqual(notifications, []);
});

for (const [name, result] of [
  ["Result error", { status: "error", error: "Reset refused" }],
  [
    "success=false",
    {
      status: "ok",
      data: { success: false, binding: null, error: "Reset refused" },
    },
  ],
] satisfies [string, Result<BindingResponse, string>][]) {
  test(`Reset ${name} notifies without refreshing and clears updating`, async () => {
    const store = setup("");
    const response = deferred<Result<BindingResponse, string>>();
    resetResponse = () => response.promise;
    const pending = store.resetBinding(rawId);
    assert.equal(
      useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
      true,
    );
    response.resolve(result);
    await pending;
    assert.equal(rawKeys(), "");
    assert.equal(
      useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
      false,
    );
    assert.deepEqual(calls, [{ name: "reset", args: [rawId] }]);
    assert.deepEqual(notifications, [errorNotification("Reset refused")]);
  });
}

test("Duplicate rejection rolls an initially empty binding back without another write", async () => {
  const store = setup("");
  const response = deferred<Result<BindingResponse, string>>();
  changeResponse = () => response.promise;
  const pending = store.updateBinding(rawId, cleanKeys);
  assert.equal(rawKeys(), cleanKeys);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    true,
  );
  const rejected = assert.rejects(pending, /already assigned to Clean/);
  response.resolve({
    status: "ok",
    data: {
      success: false,
      binding: null,
      error: "Shortcut already assigned to Clean",
    },
  });
  await rejected;
  assert.equal(rawKeys(), "");
  assert.equal(
    useSettingsStore.getState().isUpdatingKey(`binding_${rawId}`),
    false,
  );
  assert.deepEqual(calls, [{ name: "change", args: [rawId, cleanKeys] }]);
  assert.deepEqual(notifications, []);
});

test("Enable switch refusal notifies and rolls back without refresh", async () => {
  const store = setup("", true);
  const response = deferred<Result<null, string>>();
  enableResponse = () => response.promise;
  const pending = store.updateSetting("post_process_enabled", false);
  assert.equal(
    useSettingsStore.getState().settings?.post_process_enabled,
    false,
  );
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("post_process_enabled"),
    true,
  );
  response.resolve({
    status: "error",
    error: "Default Raw shortcut is already assigned",
  });
  await pending;
  assert.equal(
    useSettingsStore.getState().settings?.post_process_enabled,
    true,
  );
  assert.equal(rawKeys(), "");
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("post_process_enabled"),
    false,
  );
  assert.deepEqual(calls, [{ name: "enable", args: [false] }]);
  assert.deepEqual(notifications, [
    errorNotification("Default Raw shortcut is already assigned"),
  ]);
});

test("Successful switch refreshes the restored Raw binding", async () => {
  const store = setup("", true);
  const response = deferred<Result<null, string>>();
  enableResponse = () => response.promise;
  const pending = store.updateSetting("post_process_enabled", false);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("post_process_enabled"),
    true,
  );
  assert.equal(rawKeys(), "");
  persisted = fixture(rawDefault, false);
  response.resolve({ status: "ok", data: null });
  await pending;
  assert.equal(
    useSettingsStore.getState().settings?.post_process_enabled,
    false,
  );
  assert.equal(rawKeys(), rawDefault);
  assert.equal(
    useSettingsStore.getState().isUpdatingKey("post_process_enabled"),
    false,
  );
  assert.deepEqual(calls, [
    { name: "enable", args: [false] },
    { name: "refresh", args: [] },
  ]);
  assert.deepEqual(notifications, []);
});
