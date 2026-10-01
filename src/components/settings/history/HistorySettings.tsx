import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";
import { Check, Copy, FolderOpen, RotateCcw, Star, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  events,
  type HistoryCompareView,
  type HistoryEntry,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";
import { cleanedText, historyCopyText } from "@/lib/utils/historyPresentation";
import { formatDateTime } from "@/utils/dateFormat";
import { AudioPlayer, AudioPlayerGroup } from "../../ui/AudioPlayer";
import { Button } from "../../ui/Button";
import { Dropdown } from "../../ui/Dropdown";
import { copyToClipboard } from "./clipboard";
import { HistoryCompare } from "./HistoryCompare";
import {
  PageGeneration,
  createHistoryActions,
  createHistoryPageLoader,
  reloadHistoryOnUpdate,
  createHistoryScrollAnchor,
  createHistoryEntryActions,
  retryHistoryWithAnchor,
} from "./pageGeneration";

const IconButton: React.FC<{
  onClick: () => void;
  title: string;
  disabled?: boolean;
  active?: boolean;
  children: React.ReactNode;
}> = ({ onClick, title, disabled, active, children }) => (
  <button
    onClick={onClick}
    disabled={disabled}
    className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${
      active
        ? "text-logo-primary hover:text-logo-primary/80"
        : "text-text/50 hover:text-logo-primary"
    }`}
    title={title}
  >
    {children}
  </button>
);

interface OpenRecordingsButtonProps {
  onClick: () => void;
  label: string;
}

const OpenRecordingsButton: React.FC<OpenRecordingsButtonProps> = ({
  onClick,
  label,
}) => (
  <Button
    onClick={onClick}
    variant="secondary"
    size="sm"
    className="flex items-center gap-2"
    title={label}
  >
    <FolderOpen className="w-4 h-4" />
    <span>{label}</span>
  </Button>
);

export const HistorySettings: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { getSetting } = useSettings();
  const changeCompareView = useSettingsStore(
    (state) => state.changeHistoryCompareView,
  );
  const view = getSetting("history_compare_view") ?? "diff";
  const [actionError, setActionError] = useState<string | null>(null);
  const [clearing, setClearing] = useState(false);
  const [changingView, setChangingView] = useState(false);
  const generation = useRef(new PageGeneration());
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<HistoryEntry[]>([]);
  const loadingRef = useRef(false);

  const rootRef = useRef<HTMLDivElement>(null);
  const scrollAnchor = useMemo(
    () =>
      createHistoryScrollAnchor({
        getViewport: () => {
          const scroller = rootRef.current?.closest<HTMLElement>(
            "[data-settings-scroll]",
          );
          if (!scroller) return null;
          return {
            top: scroller.getBoundingClientRect().top,
            scrollTop: scroller.scrollTop,
            rows: Array.from(
              rootRef.current?.querySelectorAll<HTMLElement>(
                "[data-history-entry-id]",
              ) ?? [],
            ).map((element) => {
              const rect = element.getBoundingClientRect();
              return {
                id: element.dataset.historyEntryId ?? "",
                top: rect.top,
                bottom: rect.bottom,
              };
            }),
          };
        },
        setScrollTop: (scrollTop) => {
          const scroller = rootRef.current?.closest<HTMLElement>(
            "[data-settings-scroll]",
          );
          if (scroller) scroller.scrollTop = scrollTop;
        },
      }),
    [],
  );
  useLayoutEffect(() => {
    scrollAnchor.restore();
  }, [entries, scrollAnchor]);

  // Keep ref in sync for use in IntersectionObserver callback
  useLayoutEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  const {
    loadPage,
    loadPageChecked,
    applyHistoryUpdate,
    setSaved,
    removeEntry,
  } = useMemo(
    () =>
      createHistoryPageLoader({
        generation: generation.current,
        loadingRef,
        getEntries: () => entriesRef.current,
        fetchPage: commands.getHistoryEntries,
        setEntries,
        setLoading,
        setHasMore,
        setError: setActionError,
        beforeCommit: scrollAnchor.capture,
      }),
    [scrollAnchor],
  );

  const { clearHistory, changeView } = useMemo(
    () =>
      createHistoryActions({
        getSummary: commands.getHistoryClearSummary,
        clear: commands.clearHistory,
        reload: loadPageChecked,
        changeCompareView,
        confirm: (summary) =>
          ask(
            t("settings.history.clearConfirmation", {
              count: summary.entries,
              recordings: summary.recordings,
            }),
            {
              title: t("settings.history.clearHistory"),
              kind: "warning",
              okLabel: t("settings.history.clearHistory"),
              cancelLabel: t("modelSelector.cancel"),
            },
          ),
        setClearing,
        setChangingView,
        setError: setActionError,
      }),
    [changeCompareView, loadPageChecked, t],
  );

  // Initial load
  useEffect(() => {
    loadPage();
  }, [loadPage]);

  // Infinite scroll via IntersectionObserver
  useEffect(() => {
    if (loading) return;

    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;

    const observer = new IntersectionObserver(
      (observerEntries) => {
        const first = observerEntries[0];
        if (first.isIntersecting) {
          const lastEntry = entriesRef.current[entriesRef.current.length - 1];
          if (lastEntry) {
            loadPage(lastEntry.id);
          }
        }
      },
      { threshold: 0 },
    );

    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, hasMore, loadPage]);

  // Listen for new entries added from the transcription pipeline
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload = event.payload;
      if (payload.action === "cleared") scrollAnchor.clear();
      if (payload.action === "deleted") applyHistoryUpdate(payload);
      reloadHistoryOnUpdate(payload, loadPage, applyHistoryUpdate);
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, [loadPage, applyHistoryUpdate, scrollAnchor]);

  const entryActions = useMemo(
    () =>
      createHistoryEntryActions({
        getEntry: (id) => entriesRef.current.find((entry) => entry.id === id),
        setSaved,
        removeEntry,
        toggleSaved: commands.toggleHistoryEntrySaved,
        deleteEntry: commands.deleteHistoryEntry,
        reload: loadPage,
        setError: setActionError,
      }),
    [setSaved, removeEntry, loadPage],
  );

  const getAudioUrl = useCallback(
    async (fileName: string) => {
      try {
        const result = await commands.getAudioFilePath(fileName);
        if (result.status === "ok") {
          if (osType === "linux") {
            const fileData = await readFile(result.data);
            const blob = new Blob([fileData], { type: "audio/wav" });
            return URL.createObjectURL(blob);
          }
          return convertFileSrc(result.data, "asset");
        }
        return null;
      } catch (error) {
        console.error("Failed to get audio file path:", error);
        return null;
      }
    },
    [osType],
  );

  const retryHistoryEntry = async (id: number) => {
    const result = await commands.retryHistoryEntryTranscription(id);
    if (result.status !== "ok") {
      throw new Error(String(result.error));
    }
  };

  const openRecordingsFolder = async () => {
    try {
      const result = await commands.openRecordingsFolder();
      if (result.status !== "ok") {
        throw new Error(String(result.error));
      }
    } catch (error) {
      console.error("Failed to open recordings folder:", error);
    }
  };

  let content: React.ReactNode;

  if (loading) {
    content = (
      <div className="px-4 py-3 text-center text-text/60">
        {t("settings.history.loading")}
      </div>
    );
  } else if (entries.length === 0) {
    content = (
      <div className="px-4 py-3 text-center text-text/60">
        {t("settings.history.empty")}
      </div>
    );
  } else {
    content = (
      <>
        <AudioPlayerGroup>
          <div className="divide-y divide-mid-gray/20">
            {entries.map((entry) => (
              <HistoryEntryComponent
                key={entry.id}
                entry={entry}
                view={view}
                onToggleSaved={() => void entryActions.toggleSaved(entry.id)}
                onCopyText={() => copyToClipboard(historyCopyText(entry))}
                onCopyRaw={() => copyToClipboard(entry.transcription_text)}
                getAudioUrl={getAudioUrl}
                deleteAudio={entryActions.deleteEntry}
                retryTranscription={retryHistoryEntry}
                captureAnchor={scrollAnchor.capture}
                restoreAnchor={scrollAnchor.restore}
              />
            ))}
          </div>
        </AudioPlayerGroup>
        {/* Sentinel for infinite scroll */}
        <div ref={sentinelRef} className="h-1" />
      </>
    );
  }

  return (
    <div ref={rootRef} className="max-w-3xl w-full mx-auto space-y-6">
      <div className="space-y-2">
        <div className="px-4 flex flex-wrap items-center justify-between gap-2">
          <div>
            <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
              {t("settings.history.title")}
            </h2>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Dropdown
              options={(
                ["diff", "side_by_side", "stacked"] as HistoryCompareView[]
              ).map((value) => ({
                value,
                label: t(`settings.history.compare.${value}`),
              }))}
              selectedValue={view}
              onSelect={(next) => void changeView(next)}
              disabled={changingView}
              placeholder={t("settings.history.compare.title")}
            />
            <Button
              variant="secondary"
              size="sm"
              disabled={clearing}
              onClick={() => void clearHistory()}
            >
              {t("settings.history.clearHistory")}
            </Button>
            <OpenRecordingsButton
              onClick={openRecordingsFolder}
              label={t("settings.history.openFolder")}
            />
          </div>
        </div>
        {actionError && (
          <p role="alert" className="px-4 text-sm text-red-500">
            {actionError}
          </p>
        )}
        <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
          {content}
        </div>
      </div>
    </div>
  );
};

interface HistoryEntryProps {
  entry: HistoryEntry;
  view: HistoryCompareView;
  onToggleSaved: () => void;
  onCopyText: () => Promise<boolean>;
  onCopyRaw: () => Promise<boolean>;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
  captureAnchor: () => void;
  restoreAnchor: () => void;
}

const HistoryEntryComponent: React.FC<HistoryEntryProps> = ({
  entry,
  view,
  onToggleSaved,
  onCopyText,
  onCopyRaw,
  getAudioUrl,
  deleteAudio,
  retryTranscription,
  captureAnchor,
  restoreAnchor,
}) => {
  const { t, i18n } = useTranslation();
  const [showCopied, setShowCopied] = useState(false);
  const [retrying, setRetrying] = useState(false);
  useLayoutEffect(() => {
    restoreAnchor();
  }, [retrying, restoreAnchor]);

  const hasTranscription = historyCopyText(entry).trim().length > 0;

  const handleLoadAudio = useCallback(
    () => getAudioUrl(entry.file_name),
    [getAudioUrl, entry.file_name],
  );

  const handleCopyText = async () => {
    if (!hasTranscription) {
      return;
    }

    const copied = await onCopyText();
    if (!copied) {
      toast.error(t("settings.history.copyError"));
      return;
    }

    setShowCopied(true);
    setTimeout(() => setShowCopied(false), 2000);
  };

  const handleDeleteEntry = async () => {
    try {
      await deleteAudio(entry.id);
    } catch (error) {
      console.error("Failed to delete entry:", error);
      toast.error(t("settings.history.deleteError"));
    }
  };

  const handleRetranscribe = async () => {
    try {
      await retryHistoryWithAnchor(entry.id, {
        captureAnchor,
        setRetrying,
        retry: retryTranscription,
      });
    } catch (error) {
      console.error("Failed to re-transcribe:", error);
      toast.error(t("settings.history.retranscribeError"));
    }
  };

  const formattedDate = formatDateTime(String(entry.timestamp), i18n.language);

  return (
    <div
      data-history-entry-id={entry.id}
      className="px-4 py-2 pb-5 flex flex-col gap-3"
    >
      <div className="flex justify-between items-center">
        <p className="text-sm font-medium">{formattedDate}</p>
        <div className="flex items-center">
          <IconButton
            onClick={handleCopyText}
            disabled={!hasTranscription || retrying}
            title={t("settings.history.copyToClipboard")}
          >
            {showCopied ? (
              <Check width={16} height={16} />
            ) : (
              <Copy width={16} height={16} />
            )}
          </IconButton>
          <IconButton
            onClick={onToggleSaved}
            disabled={retrying}
            active={entry.saved}
            title={
              entry.saved
                ? t("settings.history.unsave")
                : t("settings.history.save")
            }
          >
            <Star
              width={16}
              height={16}
              fill={entry.saved ? "currentColor" : "none"}
            />
          </IconButton>
          <IconButton
            onClick={handleRetranscribe}
            disabled={retrying}
            title={t("settings.history.retranscribe")}
          >
            <RotateCcw
              width={16}
              height={16}
              style={
                retrying
                  ? { animation: "spin 1s linear infinite reverse" }
                  : undefined
              }
            />
          </IconButton>
          <IconButton
            onClick={handleDeleteEntry}
            disabled={retrying}
            title={t("settings.history.delete")}
          >
            <Trash2 width={16} height={16} />
          </IconButton>
        </div>
      </div>

      {!retrying && cleanedText(entry) !== null ? (
        <HistoryCompare entry={entry} view={view} onCopyRaw={onCopyRaw} />
      ) : (
        <p
          className={`italic text-sm pb-2 ${
            retrying
              ? ""
              : hasTranscription
                ? "text-text/90 select-text cursor-text whitespace-pre-wrap break-words"
                : "text-text/40"
          }`}
          style={
            retrying
              ? { animation: "transcribe-pulse 3s ease-in-out infinite" }
              : undefined
          }
        >
          {retrying && (
            <style>{`
            @keyframes transcribe-pulse {
              0%, 100% { color: color-mix(in srgb, var(--color-text) 40%, transparent); }
              50% { color: color-mix(in srgb, var(--color-text) 90%, transparent); }
            }
          `}</style>
          )}
          {retrying
            ? t("settings.history.transcribing")
            : hasTranscription
              ? entry.transcription_text
              : t("settings.history.transcriptionFailed")}
        </p>
      )}

      <AudioPlayer onLoadRequest={handleLoadAudio} className="w-full" />
    </div>
  );
};
