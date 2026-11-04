import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import english from "../../../i18n/locales/en/translation.json";
import type { TtsModelInfo } from "../../../bindings";
import { ModelSelect, selectDownloadedModel } from "./ModelSelect";
const i18n = i18next.createInstance();
await i18n.init({ lng: "en", resources: { en: { translation: english } } });
const models = [
  { id: "pocket-en", display_name: "Pocket", downloaded: false },
  { id: "supertonic-3", display_name: "Supertonic", downloaded: true },
] as TtsModelInfo[];
const render = (rows: TtsModelInfo[], value: string) =>
  renderToStaticMarkup(
    <I18nextProvider i18n={i18n}>
      <ModelSelect
        models={rows}
        value={value}
        change={() => {}}
        openModels={() => {}}
      />
    </I18nextProvider>,
  );
const html = render(models, "pocket-en");
const trigger = html.match(
  /<button[^>]*aria-expanded="false"[^>]*>(.*?)<\/button>/s,
)?.[1];
assert.ok(
  trigger?.includes("Pocket") &&
    trigger.includes(english.tts.errors.notDownloaded),
  "missing persisted selection explains download",
);
assert.ok(
  !trigger?.includes("Supertonic"),
  "downloaded model is not shown as saved before selection",
);
assert.ok(
  html.match(/<button[^>]*>Text-to-speech Models<\/button>/),
  "missing model has navigation action",
);
const empty = render(
  models.filter((model) => !model.downloaded),
  "pocket-en",
);
assert.ok(
  empty.match(/<button[^>]*disabled=""[^>]*aria-expanded/),
  "no downloaded model disables dropdown",
);
const ready = render(models, "supertonic-3");
assert.ok(
  !ready.includes(english.tts.openModels),
  "downloaded selection needs no download link",
);
const changes: string[] = [];
selectDownloadedModel(models, "pocket-en", (model) => changes.push(model.id));
selectDownloadedModel(models, "unknown", (model) => changes.push(model.id));
assert.equal(changes.length, 0, "missing models cannot be chosen");
selectDownloadedModel(models, "supertonic-3", (model) =>
  changes.push(model.id),
);
assert.deepEqual(changes, ["supertonic-3"]);
console.log(
  "model selector preserves missing selection, navigation and download guards",
);
