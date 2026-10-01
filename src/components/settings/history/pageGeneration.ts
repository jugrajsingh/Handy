import type {
  HistoryClearSummary,
  HistoryCompareView,
  HistoryEntry,
  HistoryUpdatePayload,
  PaginatedHistory,
  Result,
} from "@/bindings";

export class PageGeneration {
  private revision = 0;
  beginReset(): number {
    return ++this.revision;
  }
  current(): number {
    return this.revision;
  }
  isCurrent(revision: number): boolean {
    return revision === this.revision;
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function mergeHistoryEntries(
  previous: HistoryEntry[],
  payload: HistoryUpdatePayload,
): HistoryEntry[] {
  if (payload.action !== "added" && payload.action !== "updated")
    return previous;
  const index = previous.findIndex((entry) => entry.id === payload.entry.id);
  if (index >= 0)
    return previous.map((entry) =>
      entry.id === payload.entry.id ? payload.entry : entry,
    );
  return payload.action === "added" ? [payload.entry, ...previous] : previous;
}

export function historyAnchorScrollTop(
  beforeTop: number,
  afterTop: number,
  scrollTop: number,
): number {
  return Math.max(0, scrollTop + afterTop - beforeTop);
}

export function createHistoryScrollAnchor({
  getViewport,
  setScrollTop,
}: {
  getViewport: () => {
    top: number;
    scrollTop: number;
    rows: { id: string; top: number; bottom: number }[];
  } | null;
  setScrollTop: (scrollTop: number) => void;
}) {
  let anchor: { id: string; contentTop: number } | null = null;
  const capture = (): void => {
    if (anchor) return;
    const viewport = getViewport();
    const row = viewport?.rows.find((row) => row.bottom > viewport.top);
    if (viewport && row)
      anchor = { id: row.id, contentTop: row.top + viewport.scrollTop };
  };
  const clear = (): void => {
    anchor = null;
  };
  const restore = (): void => {
    const previous = anchor;
    clear();
    const viewport = getViewport();
    const row = viewport?.rows.find((row) => row.id === previous?.id);
    if (previous && viewport && row) {
      setScrollTop(
        historyAnchorScrollTop(
          previous.contentTop,
          row.top + viewport.scrollTop,
          viewport.scrollTop,
        ),
      );
    }
  };
  return { capture, restore, clear };
}

export function createHistoryPageLoader({
  generation,
  getEntries,
  loadingRef,
  fetchPage,
  setEntries,
  setLoading,
  setHasMore,
  setError,
  beforeCommit = () => undefined,
}: {
  beforeCommit?: () => void;
  generation: PageGeneration;
  getEntries: () => HistoryEntry[];
  loadingRef: { current: boolean };
  fetchPage: (
    cursor: number | null,
    limit: number,
  ) => Promise<Result<PaginatedHistory, string>>;
  setEntries: (update: (previous: HistoryEntry[]) => HistoryEntry[]) => void;
  setLoading: (loading: boolean) => void;
  setHasMore: (hasMore: boolean) => void;
  setError: (error: string | null) => void;
}) {
  const updates = new Map<number, HistoryEntry>();
  const addedIds = new Set<number>();
  const deletedIds = new Set<number>();
  const saved = new Map<number, boolean>();
  const removeEntry = (id: number): void => {
    updates.delete(id);
    addedIds.delete(id);
    saved.delete(id);
    deletedIds.add(id);
    beforeCommit();
    setEntries((previous) => previous.filter((entry) => entry.id !== id));
  };
  const setSaved = (id: number, value: boolean): void => {
    saved.set(id, value);
    beforeCommit();
    setEntries((previous) =>
      previous.map((entry) =>
        entry.id === id ? { ...entry, saved: value } : entry,
      ),
    );
  };
  const applyHistoryUpdate = (payload: HistoryUpdatePayload): void => {
    if (payload.action === "deleted") {
      removeEntry(payload.id);
      return;
    }
    if (payload.action !== "added" && payload.action !== "updated") return;
    if (deletedIds.has(payload.entry.id)) return;
    const savedValue = saved.get(payload.entry.id);
    const entry =
      savedValue === undefined
        ? payload.entry
        : { ...payload.entry, saved: savedValue };
    updates.set(entry.id, entry);
    if (payload.action === "added") addedIds.add(entry.id);
    if (
      payload.action === "added" ||
      getEntries().some((previous) => previous.id === entry.id)
    )
      beforeCommit();
    setEntries((previous) =>
      mergeHistoryEntries(previous, { ...payload, entry }),
    );
  };
  const loadPageChecked = async (cursor?: number): Promise<void> => {
    const isFirstPage = cursor === undefined;
    if (!isFirstPage && loadingRef.current) return;
    const revision = isFirstPage
      ? generation.beginReset()
      : generation.current();
    if (isFirstPage) {
      updates.clear();
      addedIds.clear();
      deletedIds.clear();
      saved.clear();
    }
    loadingRef.current = true;
    if (isFirstPage) setLoading(true);
    try {
      const result = await fetchPage(cursor ?? null, 30);
      if (!generation.isCurrent(revision)) return;
      if (result.status === "error") throw new Error(result.error);
      const { entries, has_more } = result.data;
      beforeCommit();
      setEntries((previous) => {
        const next = (isFirstPage ? entries : [...previous, ...entries])
          .filter((entry) => !deletedIds.has(entry.id))
          .map((entry) => updates.get(entry.id) ?? entry)
          .map((entry) =>
            saved.has(entry.id)
              ? { ...entry, saved: saved.get(entry.id) ?? entry.saved }
              : entry,
          );
        const seen = new Set<number>();
        const unique = next.filter((entry) => {
          if (seen.has(entry.id)) return false;
          seen.add(entry.id);
          return true;
        });
        const added = [...addedIds]
          .filter((id) => !seen.has(id))
          .sort((left, right) => right - left)
          .flatMap((id) => {
            const entry = updates.get(id);
            if (!entry) return [];
            return [{ ...entry, saved: saved.get(id) ?? entry.saved }];
          });
        return [...added, ...unique];
      });
      setHasMore(has_more);
    } catch (error: unknown) {
      if (!generation.isCurrent(revision)) return;
      setError(errorMessage(error));
      throw error;
    } finally {
      if (generation.isCurrent(revision)) {
        setLoading(false);
        loadingRef.current = false;
      }
    }
  };
  const loadPage = async (cursor?: number): Promise<void> => {
    try {
      await loadPageChecked(cursor);
    } catch {
      // The checked loader has already displayed the current request's error.
    }
  };
  return {
    loadPage,
    loadPageChecked,
    applyHistoryUpdate,
    setSaved,
    removeEntry,
  };
}

export function reloadHistoryOnUpdate(
  payload: HistoryUpdatePayload,
  reload: () => Promise<void>,
  merge: (payload: HistoryUpdatePayload) => void,
): void {
  if (payload.action === "added" || payload.action === "updated")
    merge(payload);
  else if (payload.action === "cleared") void reload();
}

export function createHistoryEntryActions({
  getEntry,
  setSaved,
  removeEntry,
  toggleSaved,
  deleteEntry,
  reload,
  setError,
}: {
  getEntry: (id: number) => HistoryEntry | undefined;
  setSaved: (id: number, saved: boolean) => void;
  removeEntry: (id: number) => void;
  toggleSaved: (id: number) => Promise<Result<null, string>>;
  deleteEntry: (id: number) => Promise<Result<null, string>>;
  reload: () => Promise<void>;
  setError: (error: string | null) => void;
}) {
  const savingIds = new Set<number>();
  const toggle = async (id: number): Promise<void> => {
    const entry = getEntry(id);
    if (!entry || savingIds.has(id)) return;
    savingIds.add(id);
    setError(null);
    setSaved(id, !entry.saved);
    try {
      const result = await toggleSaved(id);
      if (result.status === "error") throw new Error(result.error);
    } catch (error: unknown) {
      setSaved(id, entry.saved);
      setError(errorMessage(error));
    } finally {
      savingIds.delete(id);
    }
  };
  const remove = async (id: number): Promise<void> => {
    setError(null);
    removeEntry(id);
    try {
      const result = await deleteEntry(id);
      if (result.status === "error") throw new Error(result.error);
    } catch (error: unknown) {
      setError(errorMessage(error));
      await reload();
    }
  };
  return { toggleSaved: toggle, deleteEntry: remove };
}

export async function retryHistoryWithAnchor(
  id: number,
  {
    captureAnchor,
    setRetrying,
    retry,
  }: {
    captureAnchor: () => void;
    setRetrying: (retrying: boolean) => void;
    retry: (id: number) => Promise<void>;
  },
): Promise<void> {
  captureAnchor();
  setRetrying(true);
  try {
    await retry(id);
  } finally {
    captureAnchor();
    setRetrying(false);
  }
}

export function createHistoryActions({
  getSummary,
  confirm,
  clear,
  reload,
  changeCompareView,
  setClearing,
  setChangingView,
  setError,
}: {
  getSummary: (
    keepSaved: boolean,
  ) => Promise<Result<HistoryClearSummary, string>>;
  confirm: (summary: HistoryClearSummary) => Promise<boolean>;
  clear: (keepSaved: boolean) => Promise<Result<HistoryClearSummary, string>>;
  reload: () => Promise<void>;
  changeCompareView: (view: HistoryCompareView) => Promise<void>;
  setClearing: (clearing: boolean) => void;
  setChangingView: (changing: boolean) => void;
  setError: (error: string | null) => void;
}) {
  let clearing = false;
  let changingView = false;
  const clearHistory = async (): Promise<void> => {
    if (clearing) return;
    clearing = true;
    setClearing(true);
    setError(null);
    try {
      const summary = await getSummary(true);
      if (summary.status === "error") throw new Error(summary.error);
      if (!(await confirm(summary.data))) return;
      const result = await clear(true);
      if (result.status === "error") throw new Error(result.error);
      await reload();
    } catch (error: unknown) {
      setError(errorMessage(error));
    } finally {
      clearing = false;
      setClearing(false);
    }
  };
  const changeView = async (next: string): Promise<void> => {
    if (
      changingView ||
      (next !== "diff" && next !== "side_by_side" && next !== "stacked")
    )
      return;
    changingView = true;
    setChangingView(true);
    setError(null);
    try {
      await changeCompareView(next);
    } catch (error: unknown) {
      setError(errorMessage(error));
    } finally {
      changingView = false;
      setChangingView(false);
    }
  };
  return { clearHistory, changeView };
}
