import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import type { LocalLlmModelInfo } from "@/bindings";
import { LocalLlmModelList } from "./LocalLlmModelList";
import {
  createLocalLlmRowActions,
  rowActionState,
  type LocalLlmRowState,
} from "./localLlmRowActions";
await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const base: LocalLlmModelInfo = {
  id: "ready-model",
  display_name: "Downloaded test model",
  downloaded: true,
  attribution: "Author",
  card_url: "https://example.com",
  size_bytes: 1048576,
  prompt_style: "s1_control_line",
};
const alternatives = [
  base,
  { ...base, id: "alternative-a", display_name: "Alternative A" },
  { ...base, id: "alternative-b", display_name: "Alternative B" },
];
const missing = {
  ...base,
  id: "missing-model",
  display_name: "Missing test model",
  downloaded: false,
};
const idle: LocalLlmRowState = {
  selectedId: base.id,
  downloadId: null,
  deletingId: null,
};
const render = (model: LocalLlmModelInfo, state = idle) =>
  renderToStaticMarkup(
    <LocalLlmModelList
      models={[model]}
      {...state}
      percentage={25}
      onSelect={() => undefined}
      onDownload={() => undefined}
      onDelete={() => undefined}
    />,
  );
const buttons = (html: string) =>
  Array.from(html.matchAll(/<button\b([^>]*)>(.*?)<\/button>/g), (match) => ({
    text: match[2],
    disabled: /\sdisabled(?:=|\s|$)/.test(match[1]),
  }));
assert.ok(render(base).includes(">Downloaded test model</p>"));
assert.ok(render(base).includes(">Active</span>"));
assert.ok(render(base).includes(">Downloaded</span>"));
assert.ok(render(base).includes(">1 MiB</p>"));
assert.deepEqual(buttons(render(base)), [
  { text: "Use", disabled: true },
  { text: "Delete", disabled: false },
]);
assert.ok(render(missing).includes(">Missing test model</p>"));
assert.deepEqual(
  buttons(render(missing)),
  [
    { text: "Use", disabled: true },
    { text: "Download", disabled: false },
  ],
  "missing row has an exact idle Download button",
);
assert.deepEqual(
  buttons(render(missing, { ...idle, downloadId: missing.id })),
  [
    { text: "Use", disabled: true },
    { text: "Downloading 25%", disabled: true },
  ],
  "progress belongs to the downloading row",
);
assert.deepEqual(buttons(render(base, { ...idle, downloadId: missing.id })), [
  { text: "Use", disabled: true },
  { text: "Delete", disabled: true },
]);
for (const model of alternatives) {
  const calls: string[] = [];
  const actions = createLocalLlmRowActions(model, idle, {
    onSelect: (id) => calls.push(`use:${id}`),
    onDelete: (id) => calls.push(`delete:${id}`),
    onDownload: (id) => calls.push(`download:${id}`),
  });
  assert.equal(actions.canUse, model.id !== base.id);
  assert.equal(actions.canDelete, true);
  assert.equal(actions.canDownload, false);
  actions.use();
  actions.delete();
  actions.download();
  assert.deepEqual(
    calls,
    [
      ...(model.id !== base.id ? [`use:${model.id}`] : []),
      `delete:${model.id}`,
    ],
    `row actions use the ID of ${model.id}`,
  );
}
const calls: string[] = [];
const callbacks = {
  onSelect: (id: string) => calls.push(`use:${id}`),
  onDelete: (id: string) => calls.push(`delete:${id}`),
  onDownload: (id: string) => calls.push(`download:${id}`),
};
const missingActions = createLocalLlmRowActions(missing, idle, callbacks);
assert.equal(missingActions.canUse, false);
assert.equal(missingActions.canDelete, false);
assert.equal(missingActions.canDownload, true);
missingActions.use();
missingActions.delete();
missingActions.download();
assert.deepEqual(calls, [`download:${missing.id}`]);
for (const busyState of [
  { ...idle, downloadId: missing.id },
  { ...idle, deletingId: base.id },
]) {
  for (const model of [...alternatives, missing]) {
    calls.length = 0;
    const actions = createLocalLlmRowActions(model, busyState, callbacks);
    assert.equal(actions.canUse, false);
    assert.equal(actions.canDelete, false);
    assert.equal(actions.canDownload, false);
    actions.use();
    actions.delete();
    actions.download();
    assert.deepEqual(calls, [], "busy rows do not dispatch an action");
    assert.ok(
      buttons(render(model, busyState)).every((button) => button.disabled),
    );
  }
}
assert.equal(rowActionState(base, idle).isActive, true);
assert.equal(rowActionState(missing, idle).isActive, false);
assert.equal(
  rowActionState(missing, { ...idle, downloadId: missing.id }).isDownloading,
  true,
);
console.log("LocalLlmModelList: render and row-action assertions passed");
