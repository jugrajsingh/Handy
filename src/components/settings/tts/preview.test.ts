import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import i18next from "i18next";
import en from "../../../i18n/locales/en/translation.json";
import ja from "../../../i18n/locales/ja/translation.json";
import { PREVIEW_LANGUAGE, previewVoice } from "./actions";

const japaneseSentence = "ひとつずつ、はっきりと読み上げます。";
assert.notEqual(japaneseSentence, en.tts.previewSentence);
const resources = {
  en: { translation: en },
  ja: {
    translation: {
      ...ja,
      tts: { ...ja.tts, previewSentence: japaneseSentence },
    },
  },
};
assert.equal(PREVIEW_LANGUAGE, "en");

for (const lng of ["en", "ja"]) {
  const i18n = i18next.createInstance();
  await i18n.init({ lng, fallbackLng: "en", resources });
  const submitted: [string, string][] = [];
  const result = await previewVoice(
    i18n.t.bind(i18n),
    "alba",
    async (voice, text) => {
      submitted.push([voice, text]);
      return "spoken";
    },
  );
  assert.equal(result, "spoken");
  assert.deepEqual(
    submitted,
    [["alba", en.tts.previewSentence]],
    `ui language ${lng}`,
  );
}

const page = readFileSync(
  new URL("./SettingsPage.tsx", import.meta.url),
  "utf8",
);
assert.ok(
  page.includes("previewVoice(t, selected.voice,"),
  "preview goes through the seam",
);
assert.ok(
  !page.includes('t("tts.previewSentence"'),
  "page never resolves the sentence itself",
);
console.log(
  "voice preview speaks English under every UI language: all assertions passed",
);
