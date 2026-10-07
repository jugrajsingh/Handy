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
  tokenSet = false,
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
        available={["cpu", "gpu", "npu"]}
        state={{ selectedId, downloadId, deletingId: null }}
        actions={{ use: () => {}, download: () => {}, delete: () => {} }}
        runOn={() => {}}
        cancel={() => {}}
        tokenSet={tokenSet}
        setToken={() => {}}
      />
    </I18nextProvider>,
  );
}
function button(html: string, label: string) {
  const found = [...html.matchAll(/<button([^>]*)>(.*?)<\/button>/g)].find(
    (match) => match[2].replace(/<[^>]*>/g, "") === label,
  );
  assert.ok(found, html);
  return found[1].replace(/class="[^"]*"/g, "");
}
const active = render(model, "pocket-en");
assert.ok(active.includes('data-badge="active"'));
assert.ok(!active.includes("data-token-chip"));
assert.ok(!active.includes('data-badge="recommended"'));
assert.ok(active.includes("border-logo-primary/50 bg-logo-primary/10"));
assert.ok(!button(active, "Delete").includes("disabled"));
assert.ok(active.includes('data-tts-model="pocket-en"'));
assert.ok(active.includes(english.tts.description["pocket-en"]));
assert.ok(active.includes(i18n.t("tts.voiceCount", { count: 1 })));
assert.ok(!active.includes('data-device="gpu"'));
assert.ok(active.includes('value="1"'));
assert.ok(active.includes('max="4"'));
const preparing = active.match(/<div role="status"[^>]*>(.*?)<\/div>/);
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
assert.ok(!missing.includes(">Use</button>"));
assert.ok(!button(missing, "Download296 MB").includes("disabled"));
assert.ok(!missing.includes("<select"));
assert.ok(missing.includes('data-token-chip="required"'));
assert.ok(missing.includes('data-badge="recommended"'));
assert.ok(!button(missing, "Set token").includes("disabled"));
const tokenReady = render(
  { ...model, downloaded: false, active: false },
  "other",
  null,
  undefined,
  true,
);
assert.ok(tokenReady.includes('data-token-chip="set"'));
assert.ok(!button(tokenReady, "Download296 MB").includes("disabled"));
const supertonic = render(
  { ...model, id: "supertonic-3", downloaded: false, active: false },
  "other",
);
assert.ok(!supertonic.includes("data-token-chip"));
assert.ok(!button(supertonic, "Download296 MB").includes("disabled"));
const ready = render({ ...model, active: false }, "other");
assert.ok(!button(ready, "Use").includes("disabled"));
const select = ready.match(/<select[^>]*>(.*?)<\/select>/);
assert.ok(select);
assert.ok(select[1].includes('<option value="auto"'));
assert.ok(select[1].includes('<option value="cpu"'));
assert.ok(select[1].includes('<option value="npu"'));
assert.ok(!select[1].includes('<option value="gpu"'));
const license = active.match(/<a[^>]*>(.*?)<\/a>/);
assert.ok(license);
assert.ok(license[0].includes(`href="${model.card_url}"`));
assert.ok(license[0].includes('target="_blank"'));
assert.ok(license[0].includes('title="Voice attribution"'));
assert.ok(license[1].includes("CC-BY-4.0"));
const downloading = render(
  { ...model, downloaded: false, active: false },
  "other",
  "pocket-en",
);
assert.ok(downloading.includes('data-badge="downloading"'));
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
assert.ok(!invalidDriver.includes('data-device="npu"'));
assert.ok(invalidDriver.match(/<option value="npu"[^>]*disabled/));
assert.ok(invalidDriver.includes("NPU driver too old"));
console.log(
  "Card states: active preparing, available Pocket without/with token, available Supertonic, downloaded inactive, downloading, progress, invalid NPU passed",
);
