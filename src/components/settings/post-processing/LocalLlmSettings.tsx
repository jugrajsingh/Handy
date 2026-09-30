import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  commands,
  type LocalLlmContext,
  type LocalLlmModelInfo,
  type LocalLlmStatus,
  type LocalLlmStructure,
  type LocalLlmStyling,
} from "@/bindings";
import { Dropdown, SettingContainer } from "@/components/ui";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { useSettings } from "../../../hooks/useSettings";
import {
  initializeLocalLlmDownloadProgress,
  useLocalLlmDownloadStore,
} from "@/stores/localLlmDownloadStore";

export const LOCAL_LLM_PROVIDER_ID = "local_llm";

const STYLINGS: LocalLlmStyling[] = [
  "casual",
  "semi_casual",
  "semi_formal",
  "formal",
];
const STRUCTURES: LocalLlmStructure[] = ["prose", "lists"];
const CONTEXTS: LocalLlmContext[] = ["general", "email"];

/** Registry entries with their download state; `reload` re-reads them. */
export const useLocalLlmModels = () => {
  const [models, setModels] = useState<LocalLlmModelInfo[]>([]);
  const version = useLocalLlmDownloadStore((state) => state.version);
  const reload = useCallback(async () => {
    await commands
      .getLocalLlmModels()
      .then(setModels)
      .catch((error: unknown) => {
        console.error("Failed to load local LLM models:", error);
      });
  }, []);
  useEffect(() => {
    void initializeLocalLlmDownloadProgress().catch((error: unknown) => {
      console.error("Failed to listen for local LLM download progress:", error);
    });
  }, []);
  useEffect(() => {
    void reload();
  }, [reload, version]);
  return { models, reload };
};

export const LocalLlmSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, refreshSettings } = useSettings();
  const { models, reload } = useLocalLlmModels();
  const [status, setStatus] = useState<LocalLlmStatus | null>(null);
  const downloadModelId = useLocalLlmDownloadStore((state) => state.modelId);
  const percentage = useLocalLlmDownloadStore((state) => state.percentage);
  const downloadError = useLocalLlmDownloadStore((state) => state.error);
  const clearError = useLocalLlmDownloadStore((state) => state.clearError);
  const download = useLocalLlmDownloadStore((state) => state.download);
  const [deleting, setDeleting] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const selectedId = getSetting("local_llm_model_id") ?? null;
  const selected = models.find((m) => m.id === selectedId) ?? null;
  const busy = deleting || downloadModelId !== null;
  const progress = downloadModelId === selectedId ? percentage : null;
  const error = downloadError ?? actionError;

  useEffect(() => {
    void commands
      .getLocalLlmStatus()
      .then(setStatus)
      .catch((error: unknown) => {
        console.error("Failed to load local LLM status:", error);
      });
    const unlistenState = listen<LocalLlmStatus>(
      "local-llm-state-changed",
      (event) => setStatus(event.payload),
    ).catch((error: unknown) => {
      console.error("Failed to listen for local LLM state:", error);
      return null;
    });
    return () => {
      void unlistenState
        .then((unlisten) => unlisten?.())
        .catch((error: unknown) => {
          console.error("Failed to stop listening for local LLM state:", error);
        });
    };
  }, []);

  const handleSelect = async (id: string) => {
    clearError();
    setActionError(null);
    try {
      const result = await commands.setLocalLlmModel(id);
      if (result.status === "error") {
        setActionError(result.error);
        return;
      }
      await refreshSettings();
    } catch (error: unknown) {
      setActionError(error instanceof Error ? error.message : String(error));
    }
  };

  const handleDownload = async () => {
    clearError();
    setActionError(null);
    if (!selected || busy) return;
    await download(selected.id);
  };

  const handleDelete = async () => {
    clearError();
    setActionError(null);
    if (!selected || busy) return;
    setDeleting(true);
    try {
      const result = await commands.deleteLocalLlmModel(selected.id);
      if (result.status === "error") setActionError(result.error);
    } catch (error: unknown) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setDeleting(false);
      await reload();
    }
  };

  const statusLabel = (() => {
    if (status?.state === "ready")
      return t("settings.postProcessing.localLlm.status.ready");
    if (status?.state === "starting")
      return t("settings.postProcessing.localLlm.status.loading");
    if (status?.error)
      return t("settings.postProcessing.localLlm.status.error", {
        reason: status.error,
      });
    return t("settings.postProcessing.localLlm.status.notLoaded");
  })();

  return (
    <>
      <SettingContainer
        title={t("settings.postProcessing.localLlm.model.title")}
        description={t("settings.postProcessing.localLlm.model.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <div className="flex items-center gap-2">
          <Dropdown
            options={models.map((m) => ({
              value: m.id,
              label: m.display_name,
            }))}
            selectedValue={selectedId}
            onSelect={(value) => void handleSelect(value)}
            disabled={busy}
            placeholder={t(
              "settings.postProcessing.localLlm.model.placeholder",
            )}
          />
          {selected &&
            (selected.downloaded ? (
              <Button
                onClick={() => void handleDelete()}
                variant="secondary"
                size="md"
                disabled={busy}
              >
                {t("settings.postProcessing.localLlm.delete")}
              </Button>
            ) : (
              <Button
                onClick={() => void handleDownload()}
                variant="primary"
                size="md"
                disabled={busy}
              >
                {progress === null
                  ? t("settings.postProcessing.localLlm.download")
                  : t("settings.postProcessing.localLlm.downloading", {
                      percent: Math.round(progress),
                    })}
              </Button>
            ))}
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.postProcessing.localLlm.status.title")}
        description={t("settings.postProcessing.localLlm.status.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <span className="text-sm">{statusLabel}</span>
      </SettingContainer>

      {error && (
        <Alert variant="error" contained>
          {t("settings.postProcessing.localLlm.status.error", {
            reason: error,
          })}
        </Alert>
      )}

      {selected?.prompt_style === "s1_control_line" && (
        <>
          <SettingContainer
            title={t("settings.postProcessing.localLlm.styling.title")}
            description={t(
              "settings.postProcessing.localLlm.styling.description",
            )}
            descriptionMode="tooltip"
            layout="horizontal"
            grouped={true}
          >
            <Dropdown
              options={STYLINGS.map((value) => ({
                value,
                label: t(
                  `settings.postProcessing.localLlm.styling.options.${value}`,
                ),
              }))}
              selectedValue={getSetting("local_llm_styling") ?? "semi_formal"}
              onSelect={(value) =>
                void updateSetting(
                  "local_llm_styling",
                  value as LocalLlmStyling,
                )
              }
            />
          </SettingContainer>
          <SettingContainer
            title={t("settings.postProcessing.localLlm.structure.title")}
            description={t(
              "settings.postProcessing.localLlm.structure.description",
            )}
            descriptionMode="tooltip"
            layout="horizontal"
            grouped={true}
          >
            <Dropdown
              options={STRUCTURES.map((value) => ({
                value,
                label: t(
                  `settings.postProcessing.localLlm.structure.options.${value}`,
                ),
              }))}
              selectedValue={getSetting("local_llm_structure") ?? "prose"}
              onSelect={(value) =>
                void updateSetting(
                  "local_llm_structure",
                  value as LocalLlmStructure,
                )
              }
            />
          </SettingContainer>
          <SettingContainer
            title={t("settings.postProcessing.localLlm.context.title")}
            description={t(
              "settings.postProcessing.localLlm.context.description",
            )}
            descriptionMode="tooltip"
            layout="horizontal"
            grouped={true}
          >
            <Dropdown
              options={CONTEXTS.map((value) => ({
                value,
                label: t(
                  `settings.postProcessing.localLlm.context.options.${value}`,
                ),
              }))}
              selectedValue={getSetting("local_llm_context") ?? "general"}
              onSelect={(value) =>
                void updateSetting(
                  "local_llm_context",
                  value as LocalLlmContext,
                )
              }
            />
          </SettingContainer>
        </>
      )}

      {selected && (
        <div className="px-4 py-2 text-xs text-mid-gray">
          <button
            type="button"
            className="underline"
            onClick={() =>
              void openUrl(selected.card_url).catch((error: unknown) => {
                console.error("Failed to open local LLM model card:", error);
              })
            }
          >
            {selected.attribution}
          </button>
        </div>
      )}
    </>
  );
};
