import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import english from "../../../i18n/locales/en/translation.json";
import type { DeviceRating, TtsModelInfo } from "../../../bindings";
import { ModelCard, completeRating, applyModelUse } from "./ModelCard";
import { rowActions } from "./actions";
const i18n = i18next.createInstance();
await i18n.init({ lng: "en", resources: { en: { translation: english } } });
const rating: DeviceRating = {
  supported: true,
  placement: { main: "cpu" },
  first_audio_p95_s: 0.5,
  rtf_1x: 0.2,
  rtf_2x: 0.25,
  load_s_cold: 20,
  load_s_cached: 2,
  speed_score: 8,
  recommended: true,
};
const model: TtsModelInfo = {
  id: "pocket-en",
  display_name: "Pocket TTS",
  license: "CC-BY-4.0",
  attribution: "Voice attribution",
  card_url: "https://huggingface.co/kyutai/pocket-tts",
  size_bytes: 295738958,
  voices: ["alba"],
  default_voice: "alba",
  downloaded: true,
  downloading: false,
  npu_driver_too_old: false,
  active: true,
  rating: {
    quality: 9,
    devices: {
      cpu: rating,
      gpu: { ...rating, supported: false, recommended: false },
      npu: { ...rating, recommended: false },
    },
  },
  run_on: "auto",
  preparation: {
    state: "preparing",
    device: "npu",
    completed: 1,
    total: 4,
    graph: "decoder",
  },
};
function render(
  value: TtsModelInfo,
  selectedId: string,
  downloadId: string | null = null,
  downloadedBytes?: number,
) {
  return renderToStaticMarkup(
    <I18nextProvider i18n={i18n}>
      <ModelCard
        model={value}
        downloadProgress={
          downloadedBytes === undefined
            ? undefined
            : {
                model_id: value.id,
                downloaded_bytes: downloadedBytes,
                total_bytes: value.size_bytes,
              }
        }
        available={["cpu", "npu"]}
        state={{ selectedId, downloadId, deletingId: null }}
        actions={{ use: () => {}, download: () => {}, delete: () => {} }}
        runOn={() => {}}
        cancel={() => {}}
      />
    </I18nextProvider>,
  );
}
function button(html: string, label: string) {
  const found = html.match(new RegExp(`<button([^>]*)>${label}</button>`));
  assert.ok(found, html);
  return found[1];
}
const active = render(model, "pocket-en");
assert.ok(button(active, "Active").includes("disabled"));
assert.ok(!button(active, "Delete").includes("disabled"));
assert.ok(active.includes('data-tts-model="pocket-en"'));
assert.ok(active.includes(english.tts.description["pocket-en"]));
assert.ok(active.includes(i18n.t("tts.voiceCount", { count: 1 })));
assert.ok(!active.includes('data-device="gpu"'));
assert.ok(active.includes('value="1"'));
assert.ok(active.includes('max="4"'));
const preparing = active.match(/<div role="status">(.*?)<\/div>/);
assert.ok(preparing, "Preparing status row");
assert.ok(preparing[1].includes("Preparing for NPU: decoder"));
assert.ok(preparing[1].includes('value="1"'));
assert.ok(preparing[1].includes('max="4"'));
for (const [downloaded, selectedId, downloadId, deletingId] of [
  [false, "other", null, null],
  [true, "pocket-en", null, null],
  [true, "other", "other", null],
  [true, "other", null, "other"],
  [true, "other", null, null],
] as const) {
  let calls = 0;
  const row = rowActions(
    { id: model.id, downloaded },
    { selectedId, downloadId, deletingId },
    { use: () => calls++, download: () => {}, delete: () => {} },
  );
  applyModelUse(row);
  assert.equal(
    calls,
    downloaded &&
      selectedId !== model.id &&
      downloadId === null &&
      deletingId === null
      ? 1
      : 0,
    "composed Use callback count",
  );
}
const missing = render({ ...model, downloaded: false, active: false }, "other");
assert.ok(button(missing, "Use").includes("disabled"));
assert.ok(!button(missing, "Download").includes("disabled"));
assert.ok(missing.match(/<select[^>]*disabled/));
const downloading = render(
  { ...model, downloaded: false, active: false },
  "other",
  "pocket-en",
);
assert.ok(button(downloading, "Download").includes("disabled"));
assert.ok(!button(downloading, "Cancel").includes("disabled"));
const progress = render(
  { ...model, downloaded: false },
  "other",
  "pocket-en",
  42,
);
const downloadBar = progress.match(
  /<progress[^>]*aria-label="Model download progress"[^>]*>/,
);
assert.ok(downloadBar, progress);
assert.ok(downloadBar[0].includes('value="42"'));
assert.ok(downloadBar[0].includes('max="295738958"'));
assert.throws(
  () =>
    completeRating({
      ...model,
      rating: { quality: 9, devices: { cpu: rating } },
    }),
  /invalidRatings/,
);
console.log(
  "model card: active/download/cancel/Run on/preparing/hidden-device row assertions passed",
);

const invalidDriver = render(
  {
    ...model,
    npu_driver_too_old: true,
    preparation: { state: "ready", device: "gpu" },
  },
  "pocket-en",
);
const npuRow = invalidDriver.match(/<div data-device="npu"[^>]*>(.*?)<\/div>/);
assert.ok(npuRow, invalidDriver);
assert.ok(npuRow[1].includes("NPU driver too old"));
assert.ok(invalidDriver.match(/<option value="npu"[^>]*disabled/));
