import React from "react";
import { useTranslation } from "react-i18next";
import type { LocalLlmModelInfo } from "@/bindings";
import { Button } from "../../ui/Button";
import { createLocalLlmRowActions } from "./localLlmRowActions";

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
  return (
    <div className="divide-y divide-mid-gray/20">
      {models.map((model) => {
        const actions = createLocalLlmRowActions(
          model,
          { selectedId, downloadId, deletingId },
          { onSelect, onDelete, onDownload },
        );
        return (
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
              {actions.isActive && (
                <span className="text-xs ms-2">
                  {t("modelSelector.active")}
                </span>
              )}
            </div>
            <div className="flex gap-2">
              <Button
                variant="secondary"
                size="sm"
                disabled={!actions.canUse}
                onClick={actions.use}
              >
                {t("settings.postProcessing.localLlm.use")}
              </Button>
              {model.downloaded ? (
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={!actions.canDelete}
                  onClick={actions.delete}
                >
                  {t("settings.postProcessing.localLlm.delete")}
                </Button>
              ) : (
                <Button
                  variant="primary"
                  size="sm"
                  disabled={!actions.canDownload}
                  onClick={actions.download}
                >
                  {actions.isDownloading
                    ? t("settings.postProcessing.localLlm.downloading", {
                        percent: Math.round(percentage ?? 0),
                      })
                    : t("settings.postProcessing.localLlm.download")}
                </Button>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}
