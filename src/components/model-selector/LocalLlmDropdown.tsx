import React from "react";
import { useTranslation } from "react-i18next";
import type { LocalLlmModelInfo } from "@/bindings";

export function LocalLlmDropdown({
  models,
  selectedId,
  disabled,
  onSelect,
}: {
  models: LocalLlmModelInfo[];
  selectedId: string | null;
  disabled: boolean;
  onSelect: (id: string) => void;
}): React.JSX.Element {
  const { t } = useTranslation();
  return (
    <div className="absolute bottom-full end-0 mb-2 w-80 max-w-[calc(100vw-2rem)] max-h-[60vh] overflow-y-auto rounded-lg border border-mid-gray/20 bg-background shadow-lg py-2 z-50">
      {models
        .filter((model) => model.downloaded)
        .map((model) => (
          <button
            type="button"
            key={model.id}
            disabled={disabled}
            onClick={() => onSelect(model.id)}
            title={model.display_name}
            className={`w-full px-3 py-2 text-start hover:bg-mid-gray/10 disabled:opacity-50 ${model.id === selectedId ? "bg-logo-primary/10 text-logo-primary" : ""}`}
          >
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="text-sm text-text/80 break-words">
                  {model.display_name}
                </div>
                <div className="text-xs text-text/40 italic">
                  {t(
                    model.prompt_style === "s1_control_line"
                      ? "settings.postProcessing.localLlm.s1Description"
                      : "settings.postProcessing.localLlm.plainDescription",
                  )}
                </div>
                <div className="text-xs text-mid-gray">
                  {t("settings.postProcessing.localLlm.size", {
                    size: (model.size_bytes / 1024 / 1024).toFixed(0),
                  })}
                </div>
              </div>
              {model.id === selectedId && (
                <span className="text-xs text-logo-primary shrink-0">
                  {t("modelSelector.active")}
                </span>
              )}
            </div>
          </button>
        ))}
    </div>
  );
}
