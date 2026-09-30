import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import { HistoryCompare } from "./HistoryCompare";
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
    <HistoryCompare entry={value} view={view} onCopyRaw={async () => true} />,
  );
const diff = render("diff");
assert.match(
  diff,
  /<span class="text-xs text-logo-primary">Cleaned with historical-model<\/span>/,
);
assert.match(diff, /<button[^>]*>Copy raw<\/button>/);
assert.match(diff, /<del[^>]*>raw<\/del>/);
assert.match(diff, /<mark[^>]*>Clean<\/mark>/);
assert.match(diff, /<span>words<\/span>/);
const sideBySide = render("side_by_side");
assert.match(sideBySide, /<div class="grid grid-cols-1 sm:grid-cols-2 gap-3">/);
assert.match(sideBySide, /<h3[^>]*>Raw<\/h3><p[^>]*>raw words<\/p>/);
assert.match(sideBySide, /<h3[^>]*>Cleaned<\/h3><p[^>]*>Clean words<\/p>/);
assert.match(render("stacked"), /<div class="flex flex-col gap-3">/);
const old = renderToStaticMarkup(
  <HistoryCompare
    entry={{ ...entry, post_process_model: null }}
    view="stacked"
    onCopyRaw={async () => true}
  />,
);
assert.match(old, /<span class="text-xs text-logo-primary">Cleaned<\/span>/);
assert.ok(!old.includes("historical-model"));
for (const post_processed_text of [null, "", " \n", "raw words"]) {
  const raw = renderToStaticMarkup(
    <HistoryCompare
      entry={{ ...entry, post_processed_text }}
      view="diff"
      onCopyRaw={async () => true}
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
console.log("HistoryCompare: all assertions passed");
