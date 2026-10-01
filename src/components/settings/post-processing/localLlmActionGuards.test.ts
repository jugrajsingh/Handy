import assert from "node:assert/strict";
import type { LocalLlmModelInfo } from "@/bindings";
import { createLocalLlmActions } from "./localLlmActions";
import { createLocalLlmRowActions } from "./localLlmRowActions";

const selectedCalls: string[] = [];
const busyActions = createLocalLlmActions({
  isBusy: () => true,
  clearError: () => undefined,
  setError: () => undefined,
  setDeletingId: () => undefined,
  setLocalLlmModel: async (id) => {
    selectedCalls.push(id);
    return { status: "ok", data: null };
  },
  deleteLocalLlmModel: async () => ({ status: "ok", data: null }),
  downloadLocalLlmModel: async () => undefined,
  refreshSettingsChecked: async () => undefined,
  reloadChecked: async () => undefined,
  modelsChanged: () => undefined,
});
const readyAlternative: LocalLlmModelInfo = {
  id: "alternative",
  display_name: "Alternative",
  downloaded: true,
  attribution: "Test",
  card_url: "https://example.com",
  size_bytes: 1,
  prompt_style: "s1_control_line",
};
const guardedRow = createLocalLlmRowActions(
  readyAlternative,
  {
    selectedId: "s1-mini-q4km",
    downloadId: "qwen3-4b-instruct-2507-q4km",
    deletingId: null,
  },
  {
    onSelect: (id) => {
      void busyActions.select(id);
    },
    onDelete: () => undefined,
    onDownload: () => undefined,
  },
);
guardedRow.use();
await Promise.resolve();
assert.deepEqual(selectedCalls, []);
console.log("local LLM composed busy guards: all assertions passed");
