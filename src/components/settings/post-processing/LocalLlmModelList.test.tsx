import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import type { LocalLlmModelInfo } from "@/bindings";
import { LocalLlmModelList } from "./LocalLlmModelList";
await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const base: LocalLlmModelInfo = {
  id: "ready-model",
  display_name: "Downloaded test model",
  downloaded: true,
  attribution: "Author",
  card_url: "https://example.com",
  size_bytes: 1048576,
  prompt_style: "s1_control_line",
};
const render = (downloadId: string | null) =>
  renderToStaticMarkup(
    <LocalLlmModelList
      models={[
        base,
        {
          ...base,
          id: "missing-model",
          display_name: "Missing test model",
          downloaded: false,
        },
      ]}
      selectedId={base.id}
      downloadId={downloadId}
      percentage={25}
      deletingId={null}
      onSelect={() => undefined}
      onDownload={() => undefined}
      onDelete={() => undefined}
    />,
  );
const html = render(null);
assert.ok(html.includes("Downloaded test model"));
assert.ok(html.includes("Missing test model"));
assert.ok(html.includes("Active"));
assert.ok(html.includes("Delete"));
assert.ok(html.includes("Download"));
assert.ok(render("missing-model").includes("25"));
console.log("LocalLlmModelList: all assertions passed");
