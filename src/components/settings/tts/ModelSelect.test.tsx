import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import english from "../../../i18n/locales/en/translation.json";
import type { TtsModelInfo } from "../../../bindings";
import { ModelSelect } from "./ModelSelect";
const i18n = i18next.createInstance();
await i18n.init({ lng: "en", resources: { en: { translation: english } } });
const models = [
  { id: "pocket-en", display_name: "Pocket", downloaded: false },
  { id: "supertonic-3", display_name: "Supertonic", downloaded: true },
] as TtsModelInfo[];
const html = renderToStaticMarkup(
  <I18nextProvider i18n={i18n}>
    <ModelSelect models={models} value="pocket-en" change={() => {}} />
  </I18nextProvider>,
);
const row = html.match(/<select[^>]*>(.*?)<\/select>/)?.[1];
assert.ok(
  row?.match(/<option[^>]*value="pocket-en"[^>]*selected/),
  "missing persisted model has a selected option",
);
assert.ok(
  row?.includes(english.tts.errors.notDownloaded),
  "missing selection explains required download",
);
assert.ok(
  !row?.match(/<option[^>]*value="supertonic-3"[^>]*selected/),
  "downloaded model is not shown as saved before selection",
);
console.log("model selector preserves the actual missing selection");
