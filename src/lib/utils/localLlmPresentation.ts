import type { LocalLlmModelInfo, LocalLlmStatus, Result } from "@/bindings";

export function selectedDownloadedModel(
  models: LocalLlmModelInfo[],
  selectedId: string | null,
): LocalLlmModelInfo | null {
  return (
    models.find((model) => model.id === selectedId && model.downloaded) ?? null
  );
}

export function cleanupStatus(
  status: LocalLlmStatus | null,
): "ready" | "loading" | "error" | "unloaded" {
  switch (status?.state) {
    case "ready":
      return "ready";
    case "starting":
    case "stopping":
      return "loading";
    case "failed":
      return "error";
    default:
      return "unloaded";
  }
}

export function providerGroupKey(providerId: string): string {
  return providerId === "local_llm"
    ? "settings.postProcessing.localLlm.groupTitle"
    : "settings.postProcessing.api.title";
}

export function cleanupPickerPresentation(
  models: LocalLlmModelInfo[],
  selectedId: string | null,
  enabled: boolean | undefined,
  providerId: string | undefined,
  status: LocalLlmStatus | null,
) {
  const selected = selectedDownloadedModel(models, selectedId);
  return {
    visible: !!enabled && providerId === "local_llm",
    downloaded: models.filter((model) => model.downloaded),
    selected,
    status: selected ? cleanupStatus(status) : ("none" as const),
  };
}

export interface CleanupModelPickerDependencies {
  getModels: () => LocalLlmModelInfo[];
  getOpen: () => boolean;
  setOpen: (value: boolean) => void;
  setSelecting: (value: boolean) => void;
  setError: (message: string | null) => void;
  onOpenPostProcessing: () => void;
  setLocalLlmModel: (id: string) => Promise<Result<null, string>>;
  refreshSettingsChecked: () => Promise<void>;
  reloadChecked: () => Promise<void>;
}

export function createCleanupModelPickerActions(
  deps: CleanupModelPickerDependencies,
) {
  let selecting = false;
  const showError = (reason: unknown) => {
    deps.setError(reason instanceof Error ? reason.message : String(reason));
  };
  return {
    toggle: async () => {
      if (selecting) return;
      deps.setError(null);
      if (!deps.getModels().some((model) => model.downloaded)) {
        deps.onOpenPostProcessing();
        return;
      }
      const open = !deps.getOpen();
      deps.setOpen(open);
      if (!open) return;
      try {
        await deps.reloadChecked();
      } catch (reason: unknown) {
        showError(reason);
      }
    },
    select: async (id: string) => {
      if (selecting || !selectedDownloadedModel(deps.getModels(), id)) return;
      selecting = true;
      deps.setSelecting(true);
      deps.setError(null);
      try {
        const result = await deps.setLocalLlmModel(id);
        if (result.status === "error") throw new Error(result.error);
        await deps.refreshSettingsChecked();
        await deps.reloadChecked();
        deps.setOpen(false);
      } catch (reason: unknown) {
        showError(reason);
      } finally {
        selecting = false;
        deps.setSelecting(false);
      }
    },
  };
}
