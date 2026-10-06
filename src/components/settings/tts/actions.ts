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
}
export function selectionEntries(dependencies: SelectionDependencies) {
  let pending = false;
  const choose = async (selection: TtsSelection) => {
    if (pending) return;
    pending = true;
    try {
      await dependencies.select(selection);
      await dependencies.refresh();
    } catch {
      dependencies.error("tts.errors.selection");
    } finally {
      pending = false;
    }
  };
  return { settings: choose, tray: choose };
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
