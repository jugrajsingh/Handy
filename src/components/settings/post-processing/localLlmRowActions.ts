import type { LocalLlmModelInfo } from "@/bindings";

export interface LocalLlmRowState {
  selectedId: string | null;
  downloadId: string | null;
  deletingId: string | null;
}

export interface LocalLlmRowCallbacks {
  onSelect: (id: string) => void;
  onDelete: (id: string) => void;
  onDownload: (id: string) => void;
}

export function isLocalLlmBusy(
  state: Pick<LocalLlmRowState, "downloadId" | "deletingId">,
) {
  return state.downloadId !== null || state.deletingId !== null;
}

export function rowActionState(
  model: LocalLlmModelInfo,
  state: LocalLlmRowState,
) {
  const busy = isLocalLlmBusy(state);
  return {
    canUse: !busy && model.downloaded && model.id !== state.selectedId,
    canDelete: !busy && model.downloaded,
    canDownload: !busy && !model.downloaded,
    isActive: model.downloaded && model.id === state.selectedId,
    isDownloading: model.id === state.downloadId,
  };
}

export function createLocalLlmRowActions(
  model: LocalLlmModelInfo,
  state: LocalLlmRowState,
  callbacks: LocalLlmRowCallbacks,
) {
  const permissions = rowActionState(model, state);
  return {
    ...permissions,
    use: () => {
      if (permissions.canUse) callbacks.onSelect(model.id);
    },
    delete: () => {
      if (permissions.canDelete) callbacks.onDelete(model.id);
    },
    download: () => {
      if (permissions.canDownload) callbacks.onDownload(model.id);
    },
  };
}
