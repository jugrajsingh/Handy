import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import english from "../../../i18n/locales/en/translation.json";
import { RatingBars } from "./RatingBars";
import type { DeviceRating, ModelRating } from "./presentation";
import shippedRatings from "../../../../src-tauri/resources/tts/ratings.json";

const i18n = i18next.createInstance();
await i18n.init({
  lng: "en",
  resources: { en: { translation: english } },
  interpolation: { escapeValue: false },
});
const supported: DeviceRating = {
  supported: true,
  placement: { main: "cpu" },
  first_audio_p95_s: 0.5,
  rtf_1x: 0.25,
  rtf_2x: 0.3,
  load_s_cold: 20,
  load_s_cached: 2,
  speed_score: 8,
  recommended: true,
};
const rating: ModelRating = {
  quality: 9,
  devices: {
    cpu: { ...supported, recommended: false },
    gpu: { ...supported, supported: false, recommended: false },
    npu: supported,
  },
};
function render(
  invalidNpu: boolean,
  available: readonly ("cpu" | "gpu" | "npu")[] = ["cpu", "npu"],
  modelRating: ModelRating = rating,
  modelId = "fixture",
) {
  return renderToStaticMarkup(
    <I18nextProvider i18n={i18n}>
      <RatingBars
        modelId={modelId}
        rating={modelRating}
        available={available}
        invalidNpu={invalidNpu}
      />
    </I18nextProvider>,
  );
}
function row(html: string, device: string): string {
  const start = html.indexOf(`data-device="${device}"`);
  assert.ok(start >= 0);
  const end = html.indexOf("</div>", start);
  assert.ok(end > start);
  return html.slice(start, end);
}
const html = render(false);
assert.ok(!html.includes('data-device="gpu"'));
const npu = row(html, "npu");
assert.ok(npu.includes('data-recommended="true"'));
assert.ok(npu.includes('value="80"'));
assert.ok(npu.includes("4 times real time"));
assert.ok(npu.includes("cached load: 2 s"));
const cpu = row(html, "cpu");
assert.ok(!cpu.includes('data-recommended="true"'));
const invalid = row(render(true), "npu");
assert.ok(invalid.includes("NPU driver too old"));
assert.ok(!invalid.includes("<meter"));
assert.ok(!invalid.includes('data-recommended="true"'));
const unsupported = row(render(false, ["gpu"]), "gpu");
assert.ok(unsupported.includes("Not supported"));
assert.ok(!unsupported.includes("<meter"));
const kokoroRating = shippedRatings.models["kokoro-82m-int8"] as ModelRating;
const kokoro = row(
  render(false, ["cpu"], kokoroRating, "kokoro-82m-int8"),
  "cpu",
);
assert.ok(
  kokoro.includes(
    `First audio: ${kokoroRating.devices.cpu.first_audio_p95_s} s`,
  ),
);
assert.ok(
  kokoro.includes(
    `value="${(kokoroRating.devices.cpu.speed_score ?? 0) * 10}"`,
  ),
);
console.log("TTS row-scoped rating render assertions passed");
