import React from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { LocalLlmModelInfo } from "@/bindings";

export type OpenLink = (url: string, what: string) => void;

export const openLink: OpenLink = (url, what) =>
  void openUrl(url).catch((error: unknown) => {
    console.error(`Failed to open ${what}:`, error);
  });

export const cardAction = (model: LocalLlmModelInfo, open: OpenLink) => () =>
  open(model.card_url, "local LLM model card");

export const licenseAction = (model: LocalLlmModelInfo, open: OpenLink) => () =>
  open(model.license_url, "local LLM model license");

/** Attribution, licence link and any extra licence term, with the link opener injected. */
export const LocalLlmAttributionView = ({
  model,
  note,
  open,
}: {
  model: LocalLlmModelInfo;
  note: string | null;
  open: OpenLink;
}) => (
  <div className="px-4 py-2 text-xs text-mid-gray">
    <button
      type="button"
      className="underline"
      onClick={cardAction(model, open)}
    >
      {model.attribution}
    </button>
    <span aria-hidden="true">{" · "}</span>
    <button
      type="button"
      className="underline"
      onClick={licenseAction(model, open)}
    >
      {model.license}
    </button>
    {note && <p className="mt-1">{note}</p>}
  </div>
);

export const LocalLlmAttribution: React.FC<{ model: LocalLlmModelInfo }> = ({
  model,
}) => {
  const { t } = useTranslation();
  return (
    <LocalLlmAttributionView
      model={model}
      note={model.license_note ? t(model.license_note) : null}
      open={openLink}
    />
  );
};
