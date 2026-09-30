import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useLocalLlmStatus } from "@/hooks/useLocalLlmStatus";
import { useLocalLlmModels } from "../settings/post-processing/LocalLlmSettings";
import {
  cleanupPickerPresentation,
  createCleanupModelPickerActions,
} from "@/lib/utils/localLlmPresentation";
import ModelStatusButton from "./ModelStatusButton";

export function LocalLlmModelSelector({
  onOpenPostProcessing,
}: {
  onOpenPostProcessing: () => void;
}): React.JSX.Element | null {
  const { t } = useTranslation();
  const { getSetting, refreshSettingsChecked } = useSettings();
  const { models, reloadChecked } = useLocalLlmModels();
  const { status, error: statusError } = useLocalLlmStatus();
  const [open, setOpen] = useState(false);
  const [selecting, setSelecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const container = useRef<HTMLDivElement>(null);
  const modelsRef = useRef(models);
  const openRef = useRef(open);
  const navigationRef = useRef(onOpenPostProcessing);
  modelsRef.current = models;
  navigationRef.current = onOpenPostProcessing;
  const actions = useMemo(
    () =>
      createCleanupModelPickerActions({
        getModels: () => modelsRef.current,
        getOpen: () => openRef.current,
        setOpen: (value) => {
          openRef.current = value;
          setOpen(value);
        },
        setSelecting,
        setError,
        onOpenPostProcessing: () => navigationRef.current(),
        setLocalLlmModel: commands.setLocalLlmModel,
        refreshSettingsChecked,
        reloadChecked,
      }),
    [refreshSettingsChecked, reloadChecked],
  );
  const view = cleanupPickerPresentation(
    models,
    getSetting("local_llm_model_id") ?? null,
    getSetting("post_process_enabled"),
    getSetting("post_process_provider_id"),
    status,
  );
  useEffect(() => {
    const close = () => {
      openRef.current = false;
      setOpen(false);
    };
    const outside = (event: MouseEvent) => {
      if (
        event.target instanceof Node &&
        !container.current?.contains(event.target)
      )
        close();
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    document.addEventListener("mousedown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, []);
  if (!view.visible) return null;
  const displayedError = error ?? statusError ?? status?.error;
  return (
    <div ref={container} className="relative">
      <ModelStatusButton
        status={view.status}
        displayText={
          view.selected?.display_name ??
          t("settings.postProcessing.localLlm.noCleanupModel")
        }
        isDropdownOpen={open}
        onClick={() => void actions.toggle()}
      />
      {open && (
        <div className="absolute bottom-full start-0 mb-2 w-64 max-h-64 overflow-y-auto rounded-lg border border-mid-gray/20 bg-background shadow-lg z-50">
          {view.downloaded.map((model) => (
            <button
              type="button"
              key={model.id}
              disabled={selecting}
              onClick={() => void actions.select(model.id)}
              className="w-full flex justify-between gap-2 px-3 py-2 text-start hover:bg-mid-gray/10 disabled:opacity-50"
            >
              <span className="truncate">{model.display_name}</span>
              {model.id === view.selected?.id && (
                <span>{t("modelSelector.active")}</span>
              )}
            </button>
          ))}
        </div>
      )}
      {displayedError && (
        <p role="alert" className="text-red-500 text-xs">
          {displayedError}
        </p>
      )}
    </div>
  );
}
