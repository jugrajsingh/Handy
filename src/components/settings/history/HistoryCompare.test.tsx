import assert from "node:assert/strict";
import { createRequire } from "node:module";
const {
  mock,
}: {
  mock: {
    module: (name: string, factory: () => unknown) => void;
    restore: () => void;
  };
} = createRequire(import.meta.url)("bun:test");
import type { AppSettings, HistoryCompareView } from "@/bindings";
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
  post_process_model: "s1-mini-q4km",
};
const render = (view: "diff" | "side_by_side" | "stacked", value = entry) =>
  renderToStaticMarkup(
    <HistoryCompare
      entry={value}
      view={view}
      modelName="S1-mini by Superwhisper"
    />,
  );
const diff = render("diff");
assert.match(
  diff,
  /<span class="text-xs text-logo-primary">Post-processed with S1-mini by Superwhisper<\/span>/,
);
assert.ok(!diff.includes(entry.post_process_model));
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
for (const post_process_model of [entry.post_process_model, null]) {
  const old = renderToStaticMarkup(
    <HistoryCompare
      entry={{ ...entry, post_process_model }}
      view="stacked"
      modelName={null}
    />,
  );
  assert.match(
    old,
    /<span class="text-xs text-logo-primary">Post-processed<\/span>/,
  );
  assert.ok(!old.includes(entry.post_process_model));
  assert.ok(!old.includes("S1-mini by Superwhisper"));
}
for (const post_processed_text of [null, "", " \n"]) {
  const raw = renderToStaticMarkup(
    <HistoryCompare
      entry={{ ...entry, post_processed_text }}
      view="diff"
      modelName="S1-mini by Superwhisper"
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
    /<span class="text-xs text-logo-primary">Post-processed with S1-mini by Superwhisper<\/span>/,
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
const idleButtons = Array.from(
  actions.matchAll(/<button\b([^>]*)>/gu),
  (match) => match[1],
);
assert.ok(!idleButtons[0].includes('disabled=""'));
assert.ok(!idleButtons[1].includes('disabled=""'));
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
const rawMainButton = rawActions.match(/<button\b([^>]*)>/u)?.[1];
assert.ok(rawMainButton);
assert.ok(!rawMainButton.includes('disabled=""'));
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
mock.module("@/hooks/useOsType", () => ({ useOsType: () => "linux" }));
mock.module("@/hooks/useSettings", () => ({
  useSettings: () => ({
    getSetting: (key: keyof AppSettings) =>
      key === "history_compare_view" ? "side_by_side" : undefined,
  }),
}));
mock.module("@/stores/settingsStore", () => ({
  useSettingsStore: (
    select: (state: {
      changeHistoryCompareView: (view: HistoryCompareView) => Promise<void>;
    }) => unknown,
  ) => select({ changeHistoryCompareView: async () => undefined }),
}));
mock.module("../post-processing/LocalLlmSettings", () => ({
  useLocalLlmModels: () => ({ models: [] }),
}));
const { HistorySettings } = await import("./HistorySettings");
const history = renderToStaticMarkup(<HistorySettings />);
assert.match(
  history,
  /<div class="flex flex-wrap items-center gap-2"><span class="text-sm text-mid-gray">Compare view<\/span><div class="relative "><button[^>]*><span class="truncate">Side by side<\/span>/u,
);
mock.restore();
console.log("HistoryCompare: all assertions passed");
