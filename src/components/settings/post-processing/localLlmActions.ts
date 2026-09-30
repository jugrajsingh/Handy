import type { Result } from "@/bindings";

export interface LocalLlmActionDependencies {
  isBusy: () => boolean;
  clearError: () => void;
  setError: (message: string | null) => void;
  setDeletingId: (id: string | null) => void;
  setLocalLlmModel: (id: string) => Promise<Result<null, string>>;
  deleteLocalLlmModel: (id: string) => Promise<Result<null, string>>;
  downloadLocalLlmModel: (id: string) => Promise<void>;
  refreshSettingsChecked: () => Promise<void>;
  reloadChecked: () => Promise<void>;
  modelsChanged: () => void;
}

export function createLocalLlmActions(deps: LocalLlmActionDependencies) {
  const begin = () => {
    if (deps.isBusy()) return false;
    deps.clearError();
    deps.setError(null);
    return true;
  };
  const errorMessage = (error: unknown) =>
    error instanceof Error ? error.message : String(error);
  return {
    select: async (id: string) => {
      if (!begin()) return;
      try {
        const result = await deps.setLocalLlmModel(id);
        if (result.status === "error") throw new Error(result.error);
        await deps.refreshSettingsChecked();
      } catch (error: unknown) {
        deps.setError(errorMessage(error));
      }
    },
    download: async (id: string) => {
      if (!begin()) return;
      try {
        await deps.downloadLocalLlmModel(id);
      } catch (error: unknown) {
        deps.setError(errorMessage(error));
      }
    },
    delete: async (id: string) => {
      if (!begin()) return;
      deps.setDeletingId(id);
      try {
        const result = await deps.deleteLocalLlmModel(id);
        if (result.status === "error") throw new Error(result.error);
        deps.modelsChanged();
        await deps.reloadChecked();
        await deps.refreshSettingsChecked();
      } catch (error: unknown) {
        deps.setError(errorMessage(error));
      } finally {
        deps.setDeletingId(null);
      }
    },
  };
}
