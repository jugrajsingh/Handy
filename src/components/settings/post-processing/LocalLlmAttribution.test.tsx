import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../../../i18n/locales/en/translation.json";
import type { LocalLlmModelInfo } from "@/bindings";
import {
  LocalLlmAttribution,
  LocalLlmAttributionView,
} from "./LocalLlmAttribution";
await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const s1: LocalLlmModelInfo = {
  id: "s1-mini-q4km",
  display_name: "S1-mini by Superwhisper",
  downloaded: true,
  attribution: "S1-mini by Superwhisper",
  card_url: "https://huggingface.co/superwhisper/s1-mini-GGUF",
  license: "Apache-2.0",
  license_url:
    "https://huggingface.co/superwhisper/s1-mini-GGUF/blob/34add00a48a2e5d24e5a4ee5405a99620a3a240c/LICENSE",
  license_note: "settings.postProcessing.localLlm.licenseNotes.s1Name",
  size_bytes: 484219808,
  prompt_style: "s1_control_line",
};
const render = (model: LocalLlmModelInfo) =>
  renderToStaticMarkup(<LocalLlmAttribution model={model} />);
const note =
  "Apache-2.0 with one added term: the model must always be named &quot;S1-mini&quot; by &quot;Superwhisper&quot;, with that exact capitalization.";

const withNote = render(s1);
assert.ok(withNote.includes(">S1-mini by Superwhisper</button>"));
assert.ok(withNote.includes(">Apache-2.0</button>"));
assert.ok(withNote.includes(`<p class="mt-1">${note}</p>`));

const withoutNote = render({ ...s1, license_note: null });
assert.ok(withoutNote.includes(">Apache-2.0</button>"));
assert.ok(!withoutNote.includes("<p"));
assert.ok(!withoutNote.includes("added term"));

const buttons = (node: React.ReactNode): React.ReactElement<ButtonProps>[] => {
  if (!React.isValidElement<{ children?: React.ReactNode }>(node)) return [];
  const own =
    node.type === "button" ? [node as React.ReactElement<ButtonProps>] : [];
  return [
    ...own,
    ...React.Children.toArray(node.props.children).flatMap(buttons),
  ];
};
interface ButtonProps {
  onClick: () => void;
  children?: React.ReactNode;
}
const press = (label: string) => {
  const opened: [string, string][] = [];
  const tree = LocalLlmAttributionView({
    model: s1,
    note: null,
    open: (url, what) => opened.push([url, what]),
  });
  const found = buttons(tree).filter(
    (button) => button.props.children === label,
  );
  assert.equal(found.length, 1, `one ${label} button`);
  found[0].props.onClick();
  return opened;
};

assert.notEqual(s1.license_url, s1.card_url);
assert.deepEqual(press(s1.license), [
  [s1.license_url, "local LLM model license"],
]);
assert.deepEqual(press(s1.attribution), [
  [s1.card_url, "local LLM model card"],
]);
