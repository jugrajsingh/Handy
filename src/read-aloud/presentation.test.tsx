import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import en from "../i18n/locales/en/translation.json";
import type { SessionSnapshot } from "../bindings";
import {
  highlightedText,
  progressPresentation,
  popupMode,
  formatTime,
  pasteText,
  enterPopupEdit,
  togglePopupPlayback,
} from "./presentation";
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
const render = (state: SessionSnapshot, editing = false) =>
  renderToStaticMarkup(
    <I18nextProvider i18n={i18next}>
      <ReadAloudPopup
        text="A sentence"
        modelName="Supertonic 3"
        editing={editing}
        enterEdit={() => {}}
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
const status = buffering.match(/<span role="status"[^>]*>(.*?)<\/span>/s)?.[1];
assert.ok(status?.includes("Buffering"), status);
const mark = buffering.match(/<mark[^>]*>(.*?)<\/mark>/s)?.[1];
assert.equal(mark, "sente");
const alert = render({ ...snapshot, error: "tts.errors.notDownloaded" }).match(
  /<div role="alert"[^>]*>(.*?)<\/div>/s,
)?.[1];
assert.ok(alert?.includes(en.tts.openModels), alert);
const fallback = render({ ...snapshot, output_fallback: true });
const fallbackNote = fallback.match(
  /<span[^>]*data-output-fallback[^>]*>(.*?)<\/span>/s,
)?.[1];
assert.equal(fallbackNote, en.tts.outputFallback);
console.log(
  "popup presentation: UTF-16/highlight/Buffering/error/fallback assertions passed",
);

for (const state of [
  "waiting",
  "starting",
  "buffering",
  "playing",
  "paused",
  "finished",
  "error",
] as const) {
  const mode = ["starting", "buffering", "playing", "paused"].includes(state)
    ? "reading"
    : "editing";
  assert.equal(popupMode(state, false), mode, state);
  assert.equal(popupMode(state, true), "editing", `${state} explicit edit`);
  for (const editing of [false, true]) {
    const html = render({ ...snapshot, state }, editing);
    const readonly = html.includes("data-source-preview");
    const textarea = html.includes("<textarea");
    assert.notEqual(readonly, textarea, `${state}: exactly one text view`);
    assert.equal(
      textarea,
      editing || mode === "editing",
      `${state}: expected text view`,
    );
  }
}
assert.equal(popupMode("playing", false, "tts.errors.generic"), "editing");
for (const [seconds, expected] of [
  [0, "0:00"],
  [59, "0:59"],
  [60, "1:00"],
  [3599, "59:59"],
] as const)
  assert.equal(formatTime(seconds), expected);
assert.ok(
  buffering.match(/<span class="tabular-nums[^>]*>(.*?)<\/span>/s)?.[1] ===
    "0:09 / 0:50",
  "elapsed plus remaining gives total",
);
const progressBar = buffering.match(/<div[^>]*role="progressbar"[^>]*>/)?.[0];
assert.ok(
  progressBar?.includes('aria-label="Progress"') &&
    progressBar.includes('aria-valuemax="100"') &&
    progressBar.includes('aria-valuenow="20"'),
  progressBar,
);
const header = buffering.match(/<header[^>]*>(.*?)<\/header>/s)?.[1];
assert.ok(
  header?.includes("Supertonic 3") && header.includes('aria-label="Close"'),
);
assert.ok(
  buffering.match(/<mark[^>]*class="[^"]*bg-logo-primary/),
  "themed highlight",
);
assert.equal(pasteText("abcdef", "XY", 2, 4), "abXYef");
assert.equal(pasteText("abcdef", "new"), "new");
const edits: string[] = [];
let editingMode = false;
enterPopupEdit("keep text", {
  edit: (value) => edits.push(value),
  setEditing: (value) => {
    editingMode = value;
  },
});
assert.deepEqual(
  edits,
  ["keep text"],
  "click edit goes through revision-aware edit path",
);
assert.equal(editingMode, true);
let toggles = 0;
togglePopupPlayback({
  toggle: () => {
    toggles++;
  },
  setEditing: (value) => {
    editingMode = value;
  },
});
assert.equal(toggles, 1);
assert.equal(editingMode, false, "Play leaves edit mode");
console.log("popup mode, time, controls and edit/paste seam assertions passed");
