import React from "react";
import { useTranslation } from "react-i18next";
import type { LocalLlmModelInfo } from "@/bindings";
import { Button } from "../../ui/Button";

export interface LocalLlmModelListProps {
  models: LocalLlmModelInfo[];
  selectedId: string | null;
  downloadId: string | null;
  percentage: number | null;
  deletingId: string | null;
  onDownload: (id: string) => void;
  onDelete: (id: string) => void;
  onSelect: (id: string) => void;
}
export function LocalLlmModelList({
  models,
  selectedId,
  downloadId,
  percentage,
  deletingId,
  onDownload,
  onDelete,
  onSelect,
}: LocalLlmModelListProps) {
  const { t } = useTranslation();
  const busy = downloadId !== null || deletingId !== null;
  return (
    <div className="divide-y divide-mid-gray/20">
      {models.map((model) => (
        <div
          key={model.id}
          className="px-4 py-3 flex flex-wrap items-center justify-between gap-2"
        >
          <div className="min-w-0">
            <p className="text-sm font-medium">{model.display_name}</p>
            <p className="text-xs text-mid-gray">
              {t("settings.postProcessing.localLlm.size", {
                size: (model.size_bytes / 1024 / 1024).toFixed(0),
              })}
            </p>
            {model.downloaded && (
              <span className="text-xs">
                {t("settings.postProcessing.localLlm.downloaded")}
              </span>
            )}
            {model.downloaded && model.id === selectedId && (
              <span className="text-xs ms-2">{t("modelSelector.active")}</span>
            )}
          </div>
          <div className="flex gap-2">
            <Button
              variant="secondary"
              size="sm"
              disabled={busy || !model.downloaded || model.id === selectedId}
              onClick={() => onSelect(model.id)}
            >
              {t("settings.postProcessing.localLlm.use")}
            </Button>
            {model.downloaded ? (
              <Button
                variant="secondary"
                size="sm"
                disabled={busy}
                onClick={() => onDelete(model.id)}
              >
                {t("settings.postProcessing.localLlm.delete")}
              </Button>
            ) : (
              <Button
                variant="primary"
                size="sm"
                disabled={busy}
                onClick={() => onDownload(model.id)}
              >
                {downloadId === model.id
                  ? t("settings.postProcessing.localLlm.downloading", {
                      percent: Math.round(percentage ?? 0),
                    })
                  : t("settings.postProcessing.localLlm.download")}
              </Button>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
