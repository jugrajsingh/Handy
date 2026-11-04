import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import { createRequire } from "node:module";
const {
  mock,
  test,
}: {
  mock: { module: (name: string, factory: () => unknown) => void };
  test: (name: string, run: () => void) => void;
} = createRequire(import.meta.url)("bun:test");
mock.module("@/i18n", () => ({ default: i18next }));
const { SettingsControls } = await import("./SettingsPage");
const i18n = i18next.createInstance();
await i18n.init({ lng: "en", resources: { en: { translation: en } } });
test("settings use grouped themed controls", () => {
  const html = renderToStaticMarkup(
    <I18nextProvider i18n={i18n}>
      <SettingsControls
        selected={{
          model_id: "missing",
          voice: "alba",
          speed: 1,
          pitch: 0,
          output_device: null,
          read_code: false,
        }}
        models={[]}
        outputDevices={[]}
        error="tts.errors.generic"
        change={() => {}}
        preview={() => {}}
        openModels={() => {}}
      />
    </I18nextProvider>,
  );
  assert.ok(
    !/<select\b/.test(html),
    "settings use themed dropdowns and switch",
  );
  const checkbox = html.match(/<input[^>]*type="checkbox"[^>]*>/)?.[0];
  assert.ok(
    checkbox?.includes('class="sr-only peer"'),
    "code blocks use shared themed ToggleSwitch",
  );
  const speed = html.match(/<input[^>]*aria-label="Speed"[^>]*>/)?.[0];
  assert.ok(
    speed?.includes('class="flex-grow'),
    "speed uses shared styled Slider",
  );
  assert.ok(
    speed?.includes('min="0.5"') &&
      speed.includes('max="2"') &&
      speed.includes('step="0.1"'),
    speed,
  );
  const speedValue = html.match(
    /<input[^>]*aria-label="Speed"[^>]*>(.*?)<\/span>/s,
  )?.[1];
  assert.ok(speedValue?.includes("1.0x"));
  const pitch = html.match(/<input[^>]*aria-label="Pitch"[^>]*>/)?.[0];
  assert.ok(
    pitch?.includes('min="-6"') &&
      pitch.includes('max="6"') &&
      pitch.includes('step="1"'),
    pitch,
  );
  const titles = [...html.matchAll(/<h3[^>]*>(.*?)<\/h3>/g)].map(
    (match) => match[1],
  );
  assert.deepEqual(titles, [
    "Model",
    "Voice",
    "Speed",
    "Pitch",
    "Output device",
    "Read code blocks",
  ]);
  const alert = html.match(/<div role="alert">(.*?)<\/div>/s)?.[1];
  assert.ok(
    alert?.includes(en.tts.errors.generic) && alert.includes("bg-error-wash"),
  );
  const preview = html.match(
    /<button[^>]*disabled=""[^>]*>Preview voice<\/button>/,
  )?.[0];
  assert.ok(preview, "preview unavailable for missing model");
  console.log(
    "settings layout, themed controls, slider bounds/value and alert assertions passed",
  );
});
