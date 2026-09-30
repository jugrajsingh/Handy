import assert from "node:assert/strict";
import { createRequire } from "node:module";
import type { AppSettings, Result } from "@/bindings";

const {
  test,
  mock,
}: {
  test: (name: string, run: () => void | Promise<void>) => void;
  mock: { module: (name: string, factory: () => unknown) => void };
} = createRequire(import.meta.url)("bun:test");
let response: () => Promise<Result<AppSettings, string>>;
mock.module("@/bindings", () => ({
  commands: { getAppSettings: () => response() },
}));
mock.module("@/i18n", () => ({ default: { t: (key: string) => key } }));
mock.module("sonner", () => ({ toast: { error: () => undefined } }));
mock.module("@tauri-apps/api/event", () => ({
  listen: () => {
    throw new Error("Unexpected subscription in refresh test");
  },
}));
const { useSettingsStore } = await import("./settingsStore");

test("checked settings refresh updates the selection and retains normalization", async () => {
  response = async () => ({
    status: "ok",
    data: { local_llm_model_id: "downloaded-b" },
  });
  await useSettingsStore.getState().refreshSettingsChecked();
  assert.equal(
    useSettingsStore.getState().settings?.local_llm_model_id,
    "downloaded-b",
  );
  assert.equal(
    useSettingsStore.getState().settings?.always_on_microphone,
    false,
  );
  assert.equal(
    useSettingsStore.getState().settings?.selected_microphone,
    "Default",
  );
  assert.equal(useSettingsStore.getState().isLoading, false);
});
for (const failure of ["result", "transport"] as const) {
  test(`checked refresh rejects ${failure}; legacy refresh absorbs it`, async () => {
    const previous: AppSettings = { local_llm_model_id: "downloaded-a" };
    useSettingsStore.setState({ settings: previous, isLoading: true });
    const message = `${failure} settings failure`;
    response = async () => {
      if (failure === "transport") throw new Error(message);
      return { status: "error", error: message };
    };
    await assert.rejects(
      useSettingsStore.getState().refreshSettingsChecked(),
      new RegExp(message),
    );
    assert.equal(useSettingsStore.getState().settings, previous);
    const logged: unknown[][] = [];
    const logError = console.error;
    console.error = (...args: unknown[]) => {
      logged.push(args);
    };
    try {
      await assert.doesNotReject(useSettingsStore.getState().refreshSettings());
    } finally {
      console.error = logError;
    }
    assert.equal(logged.length, 1);
    assert.equal(useSettingsStore.getState().settings, previous);
    assert.equal(useSettingsStore.getState().isLoading, false);
  });
}
