import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import type { LocalLlmModelInfo } from "@/bindings";
import en from "../../i18n/locales/en/translation.json";
import { LocalLlmDropdown } from "../../components/model-selector/LocalLlmDropdown";
import { createCleanupModelPickerActions } from "./localLlmPresentation";

await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const model: LocalLlmModelInfo = {
  id: "s1-mini-q4km",
  display_name: "S1-mini",
  downloaded: true,
  attribution: "Superwhisper",
  card_url: "https://superwhisper.com",
  size_bytes: 484219808,
  prompt_style: "s1_control_line",
};
let releaseSelection!: () => void;
const waitSelection = new Promise<void>((resolve) => {
  releaseSelection = resolve;
});
const calls: string[] = [];
const actions = createCleanupModelPickerActions({
  getModels: () => [model],
  getOpen: () => true,
  setOpen: () => undefined,
  setSelecting: () => undefined,
  setError: () => undefined,
  onOpenPostProcessing: () => undefined,
  setLocalLlmModel: async (id) => {
    calls.push(`select:${id}`);
    return { status: "ok", data: null };
  },
  refreshSettingsChecked: () => waitSelection,
  reloadChecked: async () => undefined,
});
const firstSelection = actions.select(model.id);
await Promise.resolve();
const guardedMarkup = renderToStaticMarkup(
  React.createElement(LocalLlmDropdown, {
    models: [model],
    selectedId: model.id,
    disabled: true,
    onSelect: (id) => {
      void actions.select(id);
    },
  }),
);
const renderedButton = guardedMarkup.match(/<button\b([^>]*)>/u)?.[1];
assert.ok(renderedButton);
if (!renderedButton.includes('disabled=""')) void actions.select(model.id);
assert.equal(calls.filter((call) => call === `select:${model.id}`).length, 1);
releaseSelection();
await firstSelection;
console.log(
  "localLlmPickerGuards: composed duplicate selection assertions passed",
);
