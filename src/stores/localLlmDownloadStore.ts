import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/bindings";

type DownloadProgressPayload = {
  model_id: string;
  downloaded: number;
  total: number;
  percentage: number;
};

interface LocalLlmDownloadStore {
  modelId: string | null;
  percentage: number | null;
  error: string | null;
  version: number;
  modelsChanged: () => void;
  clearError: () => void;
  start: (modelId: string) => boolean;
  progress: (modelId: string, percentage: number) => void;
  finish: (modelId: string, error: string | null) => void;
  download: (modelId: string) => Promise<void>;
}

let progressSubscription: Promise<void> | null = null;

export const initializeLocalLlmDownloadProgress = (): Promise<void> => {
  if (!progressSubscription) {
    progressSubscription = listen<DownloadProgressPayload>(
      "local-llm-download-progress",
      ({ payload }) => {
        useLocalLlmDownloadStore
          .getState()
          .progress(payload.model_id, payload.percentage);
      },
    )
      .then(() => undefined)
      .catch((error: unknown) => {
        progressSubscription = null;
        throw error;
      });
  }
  return progressSubscription;
};

export const useLocalLlmDownloadStore = create<LocalLlmDownloadStore>(
  (set, get) => ({
    modelId: null,
    percentage: null,
    error: null,
    version: 0,

    modelsChanged: () => set((state) => ({ version: state.version + 1 })),

    clearError: () => set({ error: null }),

    start: (modelId) => {
      if (get().modelId !== null) return false;
      set({ modelId, percentage: 0, error: null });
      return true;
    },

    progress: (modelId, percentage) => {
      if (get().modelId !== modelId) return;
      set({ percentage });
    },

    finish: (modelId, error) => {
      if (get().modelId !== modelId) return;
      set((state) => ({
        modelId: null,
        percentage: null,
        error,
        version: state.version + 1,
      }));
    },

    download: async (modelId) => {
      if (!get().start(modelId)) return;
      let error: string | null = null;
      try {
        await initializeLocalLlmDownloadProgress();
        const result = await commands.downloadLocalLlmModel(modelId);
        if (result.status === "error") error = result.error;
      } catch (reason: unknown) {
        error = reason instanceof Error ? reason.message : String(reason);
      } finally {
        get().finish(modelId, error);
      }
    },
  }),
);
