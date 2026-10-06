export interface TtsSelection {
  model_id: string;
  voice: string;
  speed: number;
  pitch: number;
  output_device: string | null;
  read_code: boolean;
}
export interface SelectionDependencies {
  select: (selection: TtsSelection) => Promise<void>;
  refresh: () => Promise<void>;
  error: (key: string) => void;
  display?: (selection: TtsSelection) => void;
}
export function selectionEntries(dependencies: SelectionDependencies) {
  let pending: Promise<void> | null = null;
  let queued: TtsSelection | null = null;
  let desired: TtsSelection | null = null;
  let failed = false;
  const drain = async () => {
    try {
      while (queued !== null) {
        const selection = queued;
        queued = null;
        try {
          await dependencies.select(selection);
          await dependencies.refresh();
          failed = false;
        } catch {
          failed = true;
          dependencies.error("tts.errors.selection");
        }
      }
    } finally {
      pending = null;
    }
  };
  const choose = (selection: TtsSelection) => {
    desired = { ...selection };
    queued = desired;
    dependencies.display?.(desired);
    pending ??= drain();
    return pending;
  };
  return {
    settings: choose,
    tray: choose,
    edit: (patch: Partial<TtsSelection>, fallback: TtsSelection) =>
      choose({ ...(desired ?? fallback), ...patch }),
    sync: (selection: TtsSelection) => {
      if (pending || failed) return;
      desired = { ...selection };
      dependencies.display?.(desired);
    },
  };
}
export interface TtsRowModel {
  id: string;
  downloaded: boolean;
}
export interface TtsRowState {
  selectedId: string;
  downloadId: string | null;
  deletingId: string | null;
}
export interface TtsRowCallbacks {
  download: (id: string) => void;
  delete: (id: string) => void;
  use: (id: string) => void;
}
export function rowActions(
  model: TtsRowModel,
  state: TtsRowState,
  callbacks: TtsRowCallbacks,
) {
  const busy = state.downloadId !== null || state.deletingId !== null;
  const canUse = !busy && model.downloaded && model.id !== state.selectedId;
  const canDownload = !busy && !model.downloaded;
  const canDelete = !busy && model.downloaded;
  return {
    canUse,
    canDownload,
    canDelete,
    isActive: model.downloaded && model.id === state.selectedId,
    isDownloading: model.id === state.downloadId,
    use: () => {
      if (canUse) callbacks.use(model.id);
    },
    download: () => {
      if (canDownload) callbacks.download(model.id);
    },
    delete: () => {
      if (canDelete) callbacks.delete(model.id);
    },
  };
}

export interface TtsDownloadProgress {
  model_id: string;
  downloaded_bytes: number;
  total_bytes: number;
}
export interface DownloadSnapshot {
  active: readonly string[];
  progress: Record<string, TtsDownloadProgress>;
  error: string | null;
}
export interface DownloadDependencies {
  query: () => Promise<readonly string[]>;
  watch: (listener: (progress: TtsDownloadProgress) => void) => Promise<void>;
  download: (id: string) => Promise<void>;
  cancel: (id: string) => Promise<void>;
}
export function downloadController(dependencies: DownloadDependencies) {
  let state: DownloadSnapshot = { active: [], progress: {}, error: null };
  const subscribers = new Set<() => void>();
  const jobs = new Set<string>();
  const finished = new Set<string>();
  let watching = false;
  let revision = 0;
  const publish = (next: DownloadSnapshot) => {
    state = next;
    revision++;
    subscribers.forEach((listener) => listener());
  };
  const error = (reason: unknown) => {
    publish({
      ...state,
      error:
        reason instanceof Error && reason.message.startsWith("tts.errors.")
          ? reason.message
          : "tts.errors.generic",
    });
  };
  const refresh = async () => {
    const before = revision;
    try {
      const backend = await dependencies.query();
      if (before !== revision) return;
      backend.forEach((id) => finished.delete(id));
      const active = [...new Set([...backend, ...jobs])];
      state.active.forEach((id) => {
        if (!active.includes(id)) finished.add(id);
      });
      publish({
        ...state,
        active,
        progress: Object.fromEntries(
          Object.entries(state.progress).filter(([id]) => active.includes(id)),
        ),
      });
    } catch (reason) {
      error(reason);
    }
  };
  return {
    snapshot: () => state,
    refresh,
    subscribe: (listener: () => void) => {
      subscribers.add(listener);
      if (!watching) {
        watching = true;
        void dependencies
          .watch((progress) => {
            if (finished.has(progress.model_id)) return;
            publish({
              ...state,
              active: [...new Set([...state.active, progress.model_id])],
              progress: { ...state.progress, [progress.model_id]: progress },
            });
          })
          .catch(error);
      }
      void refresh();
      return () => {
        subscribers.delete(listener);
      };
    },
    download: async (id: string) => {
      if (state.active.length > 0) return;
      jobs.add(id);
      finished.delete(id);
      publish({ ...state, active: [id], progress: {}, error: null });
      try {
        await dependencies.download(id);
      } catch (reason) {
        error(reason);
      } finally {
        jobs.delete(id);
        finished.add(id);
        const progress = { ...state.progress };
        delete progress[id];
        publish({
          ...state,
          active: state.active.filter((model) => model !== id),
          progress,
        });
      }
    },
    cancel: async (id: string) => {
      try {
        await dependencies.cancel(id);
      } catch (reason) {
        error(reason);
      }
    },
  };
}
