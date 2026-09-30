import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  commands,
  type LocalLlmContext,
  type LocalLlmModelInfo,
  type LocalLlmStructure,
  type LocalLlmStyling,
} from "@/bindings";
import { Dropdown, SettingContainer } from "@/components/ui";
import { Alert } from "../../ui/Alert";
import { useSettings } from "../../../hooks/useSettings";
import { useLocalLlmStatus } from "../../../hooks/useLocalLlmStatus";
import { LocalLlmModelList } from "./LocalLlmModelList";
import { createLocalLlmActions } from "./localLlmActions";
import { isLocalLlmBusy } from "./localLlmRowActions";
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
  const reloadChecked = useCallback(async () => {
    setModels(await commands.getLocalLlmModels());
  }, []);
  const reload = useCallback(async () => {
    await reloadChecked().catch((error: unknown) => {
      console.error("Failed to load local LLM models:", error);
    });
  }, [reloadChecked]);
  useEffect(() => {
    void initializeLocalLlmDownloadProgress().catch((error: unknown) => {
      console.error("Failed to listen for local LLM download progress:", error);
    });
  }, []);
  useEffect(() => {
    void reload();
  }, [reload, version]);
  return { models, reload, reloadChecked };
};

export const LocalLlmSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, refreshSettingsChecked } = useSettings();
  const { models, reloadChecked } = useLocalLlmModels();
  const { status, error: statusError } = useLocalLlmStatus();
  const downloadModelId = useLocalLlmDownloadStore((state) => state.modelId);
  const percentage = useLocalLlmDownloadStore((state) => state.percentage);
  const downloadError = useLocalLlmDownloadStore((state) => state.error);
  const clearError = useLocalLlmDownloadStore((state) => state.clearError);
  const download = useLocalLlmDownloadStore((state) => state.download);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const selectedId = getSetting("local_llm_model_id") ?? null;
  const selected = models.find((m) => m.id === selectedId) ?? null;
  const error = downloadError ?? actionError ?? statusError;

  const actions = createLocalLlmActions({
    isBusy: () =>
      isLocalLlmBusy({
        downloadId: useLocalLlmDownloadStore.getState().modelId,
        deletingId,
      }),
    clearError,
    setError: setActionError,
    setDeletingId,
    setLocalLlmModel: commands.setLocalLlmModel,
    deleteLocalLlmModel: commands.deleteLocalLlmModel,
    downloadLocalLlmModel: download,
    refreshSettingsChecked,
    reloadChecked,
    modelsChanged: useLocalLlmDownloadStore.getState().modelsChanged,
  });

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
      <LocalLlmModelList
        models={models}
        selectedId={selectedId}
        downloadId={downloadModelId}
        percentage={percentage}
        deletingId={deletingId}
        onDownload={(id) => void actions.download(id)}
        onDelete={(id) => void actions.delete(id)}
        onSelect={(id) => void actions.select(id)}
      />

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
