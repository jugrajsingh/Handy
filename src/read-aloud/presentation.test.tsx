import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import en from "../i18n/locales/en/translation.json";
import type { SessionSnapshot } from "../bindings";
import { highlightedText, progressPresentation } from "./presentation";
import { ReadAloudPopup } from "./ReadAloudPopup";
await i18next.init({ lng: "en", resources: { en: { translation: en } } });
const snapshot: SessionSnapshot = {
  epoch: 0,
  state: "buffering",
  source_start: 2,
  source_end: 7,
  elapsed_seconds: 9.4,
  remaining_seconds: 40.6,
  progress: 0.2,
  error: null,
  output_fallback: false,
  queued_audio_seconds: 1,
  buffering_pauses: 1,
  head_start_seconds: 4,
};
assert.deepEqual(highlightedText("A😀BC", 1, 3), {
  before: "A",
  current: "😀",
  after: "BC",
});
assert.deepEqual(highlightedText("abc", -1, 99), {
  before: "",
  current: "abc",
  after: "",
});
assert.equal(progressPresentation({ ...snapshot, progress: 2 }).percent, 100);
const render = (state: SessionSnapshot) =>
  renderToStaticMarkup(
    <I18nextProvider i18n={i18next}>
      <ReadAloudPopup
        text="A sentence"
        snapshot={state}
        edit={() => {}}
        toggle={() => {}}
        stop={() => {}}
        close={() => {}}
        models={() => {}}
      />
    </I18nextProvider>,
  );
const buffering = render(snapshot);
const status = buffering.match(/<span role="status">(.*?)<\/span>/s)?.[1];
assert.ok(status?.includes("Buffering"), status);
const mark = buffering.match(/<mark>(.*?)<\/mark>/s)?.[1];
assert.equal(mark, "sente");
const alert = render({ ...snapshot, error: "tts.errors.notDownloaded" }).match(
  /<div role="alert">(.*?)<\/div>/s,
)?.[1];
assert.ok(alert?.includes(en.tts.openModels), alert);
const fallback = render({ ...snapshot, output_fallback: true });
assert.ok(fallback.includes(String(en.tts.outputFallback)));
console.log(
  "popup presentation: UTF-16/highlight/Buffering/error/fallback assertions passed",
);
