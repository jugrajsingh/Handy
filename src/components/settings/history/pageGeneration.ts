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

export function createHistoryPageLoader({
  generation,
  loadingRef,
  fetchPage,
  setEntries,
  setLoading,
  setHasMore,
  setError,
}: {
  generation: PageGeneration;
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
  const loadPageChecked = async (cursor?: number): Promise<void> => {
    const isFirstPage = cursor === undefined;
    if (!isFirstPage && loadingRef.current) return;
    const revision = isFirstPage
      ? generation.beginReset()
      : generation.current();
    loadingRef.current = true;
    if (isFirstPage) setLoading(true);
    try {
      const result = await fetchPage(cursor ?? null, 30);
      if (!generation.isCurrent(revision)) return;
      if (result.status === "error") throw new Error(result.error);
      const { entries, has_more } = result.data;
      setEntries((previous) =>
        isFirstPage ? entries : [...previous, ...entries],
      );
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
  return { loadPage, loadPageChecked };
}

export function reloadHistoryOnUpdate(
  payload: Pick<HistoryUpdatePayload, "action">,
  reload: () => Promise<void>,
): void {
  if (
    payload.action === "added" ||
    payload.action === "updated" ||
    payload.action === "cleared"
  ) {
    void reload();
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
