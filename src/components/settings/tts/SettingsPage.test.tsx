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
// Records every switch's props and renders the real switch as well.
const { Switch: RealSwitch } = await import("@/components/ui/Switch");
const switches: {
  checked: boolean;
  disabled?: boolean;
  onChange: (checked: boolean) => void;
}[] = [];
mock.module("@/components/ui/Switch", () => ({
  Switch: (props: Parameters<typeof RealSwitch>[0]) => {
    switches.push(props);
    return RealSwitch(props);
  },
}));
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
          technical: true,
          full_paths: true,
          full_links: false,
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
    "Technical reading",
    "Read full paths",
    "Read full links",
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

const baseSelection = {
  model_id: "missing",
  voice: "alba",
  speed: 1,
  pitch: 0,
  output_device: null,
  read_code: false,
};
function renderSwitches(selected: Record<string, unknown>) {
  switches.length = 0;
  const changes: Record<string, unknown>[] = [];
  const html = renderToStaticMarkup(
    <I18nextProvider i18n={i18n}>
      <SettingsControls
        selected={{ ...baseSelection, ...selected }}
        models={[]}
        outputDevices={[]}
        error={null}
        change={(patch) => changes.push(patch)}
        preview={() => {}}
        openModels={() => {}}
      />
    </I18nextProvider>,
  );
  // Read code blocks, Technical reading, Read full paths, Read full links.
  assert.equal(switches.length, 4);
  return {
    html,
    changes,
    readCode: switches[0],
    technical: switches[1],
    paths: switches[2],
    links: switches[3],
  };
}
test("technical reading toggle renders checked from the selection and sends technical", () => {
  const on = renderSwitches({
    technical: true,
    full_paths: true,
    full_links: false,
  });
  assert.equal(on.technical.checked, true);
  assert.equal(on.paths.checked, true);
  assert.equal(on.links.checked, false);
  on.technical.onChange(false);
  assert.deepEqual(on.changes, [{ technical: false }]);
  const off = renderSwitches({
    technical: false,
    full_paths: true,
    full_links: false,
  });
  assert.equal(off.technical.checked, false);
  off.technical.onChange(true);
  assert.deepEqual(off.changes, [{ technical: true }]);
  console.log("technical reading toggle assertions passed");
});
test("full paths and full links send their fields and grey out when technical reading is off", () => {
  const on = renderSwitches({
    technical: true,
    full_paths: false,
    full_links: true,
  });
  assert.equal(on.paths.checked, false);
  assert.equal(on.links.checked, true);
  assert.ok(!on.paths.disabled && !on.links.disabled);
  on.paths.onChange(true);
  on.links.onChange(false);
  assert.deepEqual(on.changes, [{ full_paths: true }, { full_links: false }]);
  const off = renderSwitches({
    technical: false,
    full_paths: true,
    full_links: false,
  });
  assert.equal(off.paths.disabled, true);
  assert.equal(off.links.disabled, true);
  assert.ok(!off.technical.disabled && !off.readCode.disabled);
  console.log("full paths and links toggle assertions passed");
});
