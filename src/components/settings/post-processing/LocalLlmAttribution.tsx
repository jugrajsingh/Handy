import React from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { LocalLlmModelInfo } from "@/bindings";

const openLink = (url: string, what: string) =>
  void openUrl(url).catch((error: unknown) => {
    console.error(`Failed to open ${what}:`, error);
  });

/** Attribution, licence link and any extra licence term for the selected model. */
export const LocalLlmAttribution: React.FC<{ model: LocalLlmModelInfo }> = ({
  model,
}) => {
  const { t } = useTranslation();
  return (
    <div className="px-4 py-2 text-xs text-mid-gray">
      <button
        type="button"
        className="underline"
        onClick={() => openLink(model.card_url, "local LLM model card")}
      >
        {model.attribution}
      </button>
      <span aria-hidden="true">{" · "}</span>
      <button
        type="button"
        className="underline"
        onClick={() => openLink(model.license_url, "local LLM model license")}
      >
        {model.license}
      </button>
      {model.license_note && <p className="mt-1">{t(model.license_note)}</p>}
    </div>
  );
};
