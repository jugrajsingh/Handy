import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import { HistoryCompare } from "./HistoryCompare";
import { HistoryActions } from "./HistoryActions";
await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const entry = {
  transcription_text: "raw words",
  post_processed_text: "Clean words",
  post_process_model: "historical-model",
};
const render = (view: "diff" | "side_by_side" | "stacked", value = entry) =>
  renderToStaticMarkup(
    <HistoryCompare entry={value} view={view} modelName="historical-model" />,
  );
const diff = render("diff");
assert.match(
  diff,
  /<span class="text-xs text-logo-primary">Post-processed with historical-model<\/span>/,
);
assert.ok(!diff.includes("<button"));
assert.match(diff, /<del[^>]*>raw<\/del>/);
assert.match(diff, /<mark[^>]*>Clean<\/mark>/);
assert.match(diff, /<span>words<\/span>/);
const sideBySide = render("side_by_side");
assert.match(sideBySide, /<div class="grid grid-cols-1 sm:grid-cols-2 gap-3">/);
assert.match(sideBySide, /<h3[^>]*>Raw<\/h3><p[^>]*>raw words<\/p>/);
assert.match(
  sideBySide,
  /<h3[^>]*>Post-processed<\/h3><p[^>]*>Clean words<\/p>/,
);
assert.match(render("stacked"), /<div class="flex flex-col gap-3">/);
const old = renderToStaticMarkup(
  <HistoryCompare
    entry={{ ...entry, post_process_model: null }}
    view="stacked"
    modelName={null}
  />,
);
assert.match(
  old,
  /<span class="text-xs text-logo-primary">Post-processed<\/span>/,
);
assert.ok(!old.includes("historical-model"));
for (const post_processed_text of [null, "", " \n"]) {
  const raw = renderToStaticMarkup(
    <HistoryCompare
      entry={{ ...entry, post_processed_text }}
      view="diff"
      modelName="historical-model"
    />,
  );
  assert.match(raw, /^<p[^>]*>raw words<\/p>$/);
}
for (const longSide of ["transcription_text", "post_processed_text"] as const) {
  const long = render("diff", {
    ...entry,
    [longSide]: Array(4001).fill("word").join(" "),
  });
  assert.match(long, /<div class="grid grid-cols-1 sm:grid-cols-2 gap-3">/);
  assert.ok(!long.includes("<del"));
  assert.ok(!long.includes("<mark"));
}
for (const view of ["diff", "side_by_side", "stacked"] as const) {
  const equal = render(view, {
    ...entry,
    post_processed_text: entry.transcription_text,
  });
  assert.match(
    equal,
    /<span class="text-xs text-logo-primary">Post-processed with historical-model<\/span>/,
  );
  assert.equal((equal.match(/<p\b/gu) ?? []).length, 1);
  assert.match(equal, /<p[^>]*>raw words<\/p>/);
  assert.ok(!equal.includes("<del"));
  assert.ok(!equal.includes("<mark"));
  assert.ok(!equal.includes("<button"));
  assert.ok(!equal.includes("<h3"));
}
const callbacks = {
  onCopy: () => undefined,
  onCopyRaw: () => undefined,
  onToggleSaved: () => undefined,
  onRetranscribe: () => undefined,
  onDelete: () => undefined,
};
const actions = renderToStaticMarkup(
  <HistoryActions
    processed
    rawAvailable
    hasText
    saved={false}
    retrying={false}
    copied={false}
    {...callbacks}
  />,
);
const titles = Array.from(
  actions.matchAll(/<button[^>]*title="([^"]+)"/gu),
  (match) => match[1],
);
assert.deepEqual(titles, [
  "Copy post-processed text",
  "Copy raw text",
  "Save transcription",
  "Re-transcribe",
  "Delete entry",
]);
assert.match(actions, /^<div class="flex items-center"/u);
assert.equal((actions.match(/<button\b/gu) ?? []).length, 5);
const rawActions = renderToStaticMarkup(
  <HistoryActions
    processed={false}
    rawAvailable
    hasText
    saved={false}
    retrying={false}
    copied={false}
    {...callbacks}
  />,
);
assert.equal((rawActions.match(/<button\b/gu) ?? []).length, 4);
assert.ok(!rawActions.includes('title="Copy raw text"'));
assert.match(
  rawActions,
  /<button[^>]*title="Copy transcription to clipboard"/u,
);
const busy = renderToStaticMarkup(
  <HistoryActions
    processed
    rawAvailable
    hasText
    saved
    retrying
    copied={false}
    {...callbacks}
  />,
);
const busyButtons = Array.from(
  busy.matchAll(/<button\b([^>]*)>/gu),
  (match) => match[1],
);
assert.equal(busyButtons.length, 5);
assert.ok(busyButtons.every((button) => button.includes('disabled=""')));
const unavailable = renderToStaticMarkup(
  <HistoryActions
    processed
    rawAvailable={false}
    hasText={false}
    saved={false}
    retrying={false}
    copied={false}
    {...callbacks}
  />,
);
const unavailableButtons = Array.from(
  unavailable.matchAll(/<button\b([^>]*)>/gu),
  (match) => match[1],
);
assert.ok(unavailableButtons[0].includes('disabled=""'));
assert.ok(unavailableButtons[1].includes('disabled=""'));
assert.ok(
  unavailableButtons
    .slice(2)
    .every((button) => !button.includes('disabled=""')),
);
console.log("HistoryCompare: all assertions passed");
