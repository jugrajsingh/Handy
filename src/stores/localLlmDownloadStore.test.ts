import assert from "node:assert/strict";
import { createRequire } from "node:module";
const {
  test,
  mock,
}: {
  test: (name: string, run: () => void | Promise<void>) => void;
  mock: { module: (name: string, factory: () => unknown) => void };
} = createRequire(import.meta.url)("bun:test");
import type { LocalLlmModelInfo, Result } from "@/bindings";
import { createCleanupModelPickerActions } from "@/lib/utils/localLlmPresentation";

let finish!: (result: Result<null, string>) => void;
const pending = new Promise<Result<null, string>>((resolve) => {
  finish = resolve;
});
const downloads: string[] = [];
mock.module("@tauri-apps/api/event", () => ({
  listen: async () => () => undefined,
}));
mock.module("@/bindings", () => ({
  commands: {
    downloadLocalLlmModel: async (id: string) => {
      downloads.push(id);
      return pending;
    },
  },
}));
const { useLocalLlmDownloadStore } = await import("./localLlmDownloadStore");

test("Qwen download remains owned after footer selection changes", async () => {
  const qwen = "qwen3-4b-instruct-2507-q4km";
  const alternative: LocalLlmModelInfo = {
    id: "s1-mini-q4km",
    display_name: "S1-mini by Superwhisper",
    downloaded: true,
    attribution: "Superwhisper",
    card_url: "https://huggingface.co/superwhisper/s1-mini-GGUF",
    size_bytes: 484219808,
    prompt_style: "s1_control_line",
  };
  const store = useLocalLlmDownloadStore;
  store.setState({ modelId: null, percentage: null, error: null, version: 0 });
  const work = store.getState().download(qwen);
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(store.getState().modelId, qwen);
  assert.equal(store.getState().start("other-download"), false);
  store.getState().progress(qwen, 35);
  store.getState().progress("wrong-id", 99);
  assert.equal(store.getState().percentage, 35);
  store.getState().finish("wrong-id", "wrong failure");
  assert.equal(store.getState().modelId, qwen);
  assert.equal(store.getState().version, 0);
  let selected: string | null = qwen;
  const calls: string[] = [];
  const picker = createCleanupModelPickerActions({
    getModels: () => [alternative],
    getOpen: () => true,
    setOpen: () => undefined,
    setSelecting: () => undefined,
    setError: (message) => {
      assert.equal(message, null);
    },
    onOpenPostProcessing: () => {
      throw new Error("unexpected navigation");
    },
    setLocalLlmModel: async (id) => {
      selected = id;
      calls.push(id);
      return { status: "ok", data: null };
    },
    refreshSettingsChecked: async () => undefined,
    reloadChecked: async () => undefined,
  });
  await picker.select(alternative.id);
  assert.equal(selected, alternative.id);
  assert.equal(store.getState().modelId, qwen);
  finish({ status: "ok", data: null });
  await work;
  assert.deepEqual(downloads, [qwen]);
  assert.deepEqual(calls, [alternative.id]);
  assert.equal(selected, alternative.id);
  assert.equal(store.getState().modelId, null);
  assert.equal(store.getState().percentage, null);
  assert.equal(store.getState().version, 1);
});
