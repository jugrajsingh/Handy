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
import {
  Check,
  Copy,
  FileAudio,
  FilePlus,
  FolderOpen,
  RotateCcw,
  Star,
  Trash2,
  X,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { notify } from "@/stores/noticeStore";
import {
  commands,
  events,
  type HistoryCompareView,
  type HistoryEntry,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import {
  useTranscriptionJobsStore,
  type ActiveJobStatus,
} from "@/stores/transcriptionJobsStore";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";
import { cleanedText, historyCopyText } from "@/lib/utils/historyPresentation";
import { formatDateTime } from "@/utils/dateFormat";
import { AudioPlayer, AudioPlayerGroup } from "../../ui/AudioPlayer";
import { Button } from "../../ui/Button";
import { Dropdown } from "../../ui/Dropdown";
import { copyToClipboard } from "./clipboard";
import {
  pickAndTranscribeFiles,
  useFileReadsStore,
  type FileRead,
} from "./transcribeFiles";
import { HistoryCompare } from "./HistoryCompare";
import {
  PageGeneration,
  createHistoryActions,
  createHistoryPageLoader,
  reloadHistoryOnUpdate,
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
        ? "text-logo-primary hover:text-logo-primary"
        : "text-ink-3 hover:text-logo-primary"
    }`}
    title={title}
  >
    {children}
  </button>
);

const PAGE_SIZE = 30;

// Long text folds to this many lines until "Show more".
const FOLDED_LINES = 4;

// The pulse of an entry's text while it's being transcribed.
const TRANSCRIBE_PULSE = {
  animation: "transcribe-pulse 3s ease-in-out infinite",
};

const TranscribePulseKeyframes: React.FC = () => (
  <style>{`
    @keyframes transcribe-pulse {
      0%, 100% { color: var(--color-ink-3); }
      50% { color: var(--color-text); }
    }
  `}</style>
);

// A dropped or picked file while its audio is read, before it's an entry.
const FileReadRow: React.FC<{ read: FileRead }> = ({ read }) => {
  const { t } = useTranslation();
  return (
    <div className="px-4 py-2 pb-5 flex flex-col gap-3">
      <span
        className="flex items-center gap-1 min-w-0 h-7 text-sm text-ink-3"
        title={read.name}
      >
        <FileAudio width={14} height={14} className="shrink-0" />
        <span className="truncate">{read.name}</span>
      </span>
      <p className="italic text-sm w-full" style={TRANSCRIBE_PULSE}>
        <TranscribePulseKeyframes />
        {t("settings.history.readingFile")}
      </p>
    </div>
  );
};

interface HeaderButtonProps {
  onClick: () => void;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}

const HeaderButton: React.FC<HeaderButtonProps> = ({
  onClick,
  label,
  icon: Icon,
}) => (
  <Button
    onClick={onClick}
    variant="secondary"
    size="sm"
    className="flex items-center gap-2"
    title={label}
  >
    <Icon className="w-4 h-4" />
    <span>{label}</span>
  </Button>
);

export const HistorySettings: React.FC = () => {
  const { t } = useTranslation();
  const osType = useOsType();
  const jobs = useTranscriptionJobsStore((state) => state.jobs);
  const reads = useFileReadsStore((state) => state.reads);
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

  // Keep ref in sync for use in IntersectionObserver callback
  useEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  const { loadPage, loadPageChecked } = useMemo(
    () =>
      createHistoryPageLoader({
        generation: generation.current,
        loadingRef,
        fetchPage: commands.getHistoryEntries,
        setEntries,
        setLoading,
        setHasMore,
        setError: setActionError,
      }),
    [],
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
              cancelLabel: t("common.cancel"),
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
      reloadHistoryOnUpdate(event.payload, loadPage);
      // "deleted" and "toggled" are handled by optimistic updates only,
      // so we intentionally ignore them here to avoid double-mutation.
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, [loadPage]);

  const toggleSaved = async (id: number) => {
    // Optimistic update
    setEntries((prev) =>
      prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
    );
    try {
      const result = await commands.toggleHistoryEntrySaved(id);
      if (result.status !== "ok") {
        // Revert on failure
        setEntries((prev) =>
          prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
        );
      }
    } catch (error) {
      console.error("Failed to toggle saved status:", error);
      // Revert on failure
      setEntries((prev) =>
        prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
      );
    }
  };

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

  const deleteAudioEntry = async (id: number) => {
    // Optimistically remove
    setEntries((prev) => prev.filter((e) => e.id !== id));
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") {
        throw new Error(String(result.error));
      }
    } catch (error) {
      console.error("Failed to delete entry:", error);
      loadPage();
      throw error;
    }
  };

  const transcribeFile = async () => {
    try {
      await pickAndTranscribeFiles();
    } catch (error) {
      console.error("Failed to pick a file to transcribe:", error);
      notify({ title: t("settings.history.transcribeFailed") });
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
      <div className="px-4 py-3 text-center text-ink-2">
        {t("settings.history.loading")}
      </div>
    );
  } else if (entries.length === 0 && reads.length === 0) {
    content = (
      <div className="px-4 py-3 text-center text-ink-2">
        {t("settings.history.empty")}
      </div>
    );
  } else {
    content = (
      <>
        <AudioPlayerGroup>
          <div className="divide-y divide-line">
            {reads.map((read) => (
              <FileReadRow key={`read-${read.id}`} read={read} />
            ))}
            {entries.map((entry) => (
              <HistoryEntryComponent
                key={entry.id}
                entry={entry}
                job={jobs[entry.id]}
                view={view}
                onToggleSaved={() => toggleSaved(entry.id)}
                onCopyText={() => copyToClipboard(historyCopyText(entry))}
                onCopyRaw={() => copyToClipboard(entry.transcription_text)}
                getAudioUrl={getAudioUrl}
                deleteAudio={deleteAudioEntry}
                retryTranscription={commands.retryHistoryEntryTranscription}
                cancelTranscription={commands.cancelHistoryEntryTranscription}
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
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div className="space-y-2">
        <div className="px-4 flex flex-wrap items-center justify-between gap-2">
          <div>
            <h2 className="text-xs font-medium text-ink-3 uppercase tracking-wide">
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
            <HeaderButton onClick={transcribeFile} label={t("settings.history.transcribeFile")} icon={FilePlus} />
            <HeaderButton
              onClick={openRecordingsFolder}
              label={t("settings.history.openFolder")}
              icon={FolderOpen}
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
  job?: ActiveJobStatus;
  view: HistoryCompareView;
  onToggleSaved: () => void;
  onCopyText: () => Promise<boolean>;
  onCopyRaw: () => Promise<boolean>;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
  cancelTranscription: (id: number) => Promise<void>;
}

const HistoryEntryComponent: React.FC<HistoryEntryProps> = ({
  entry,
  job,
  view,
  onToggleSaved,
  onCopyText,
  onCopyRaw,
  getAudioUrl,
  deleteAudio,
  retryTranscription,
  cancelTranscription,
}) => {
  const { t, i18n } = useTranslation();
  const [showCopied, setShowCopied] = useState(false);
  // Pressed cancel: a running job stops after its current chunk.
  const [cancelling, setCancelling] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [overflows, setOverflows] = useState(false);
  const textRef = useRef<HTMLParagraphElement>(null);

  // Queued or being transcribed in the background.
  const retrying = job !== undefined;
  useEffect(() => {
    if (!retrying) setCancelling(false);
  }, [retrying]);
  const hasTranscription = historyCopyText(entry).trim().length > 0;
  const folded = hasTranscription && !retrying && !expanded;

  // Offer "Show more" only when the folded text is actually cut off.
  useLayoutEffect(() => {
    const el = textRef.current;
    if (!el || !folded) return;
    const measure = () => setOverflows(el.scrollHeight > el.clientHeight + 1);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [folded, entry.transcription_text]);

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
      notify({ title: t("settings.history.copyError") });
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
      notify({ title: t("settings.history.deleteError") });
    }
  };

  const formattedDate = formatDateTime(String(entry.timestamp), i18n.language);

  let progressText = t("settings.history.transcribing");
  if (job?.state === "queued") {
    progressText = t("settings.history.waiting");
  } else if (job?.state === "running" && job.percent > 0) {
    progressText = t("settings.history.transcribingPercent", {
      percent: job.percent,
    });
  }

  return (
    <div className="px-4 py-2 pb-5 flex flex-col gap-3">
      <div className="flex justify-between items-center gap-3">
        <div className="flex items-center gap-2 min-w-0">
          <p className="text-sm font-medium shrink-0">{formattedDate}</p>
          {entry.source_name && (
            <span
              className="flex items-center gap-1 min-w-0 text-sm text-ink-3"
              title={entry.source_name}
            >
              <FileAudio width={14} height={14} className="shrink-0" />
              <span className="truncate">{entry.source_name}</span>
            </span>
          )}
        </div>
        <div className="flex items-center shrink-0">
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
          {retrying ? (
            <IconButton
              onClick={() => {
                setCancelling(true);
                void cancelTranscription(entry.id);
              }}
              disabled={cancelling}
              title={t("settings.history.cancelTranscription")}
            >
              <X width={16} height={16} />
            </IconButton>
          ) : (
            <IconButton
              onClick={() => retryTranscription(entry.id)}
              title={t("settings.history.retranscribe")}
            >
              <RotateCcw width={16} height={16} />
            </IconButton>
          )}
          <IconButton
            onClick={handleDeleteEntry}
            title={t("settings.history.delete")}
          >
            <Trash2 width={16} height={16} />
          </IconButton>
        </div>
      </div>

      {!retrying && cleanedText(entry) !== null ? (
        <HistoryCompare entry={entry} view={view} onCopyRaw={onCopyRaw} />
      ) : (
      <div className="flex flex-col items-start gap-1 pb-2">
        <p
          ref={textRef}
          className={`italic text-sm w-full ${
            retrying
              ? ""
              : hasTranscription
                ? "text-text select-text cursor-text whitespace-pre-wrap break-words"
                : "text-ink-3"
          }`}
          style={
            retrying
              ? TRANSCRIBE_PULSE
              : folded
                ? {
                    display: "-webkit-box",
                    WebkitLineClamp: FOLDED_LINES,
                    WebkitBoxOrient: "vertical",
                    overflow: "hidden",
                  }
                : undefined
          }
        >
          {retrying && <TranscribePulseKeyframes />}
          {retrying
            ? progressText
            : hasTranscription
              ? entry.transcription_text
              : entry.source_name
                ? // Cancelled, interrupted by a quit, or failed.
                  t("settings.history.notTranscribed")
                : t("settings.history.transcriptionFailed")}
        </p>
        {hasTranscription && !retrying && (expanded || overflows) && (
          <button
            type="button"
            onClick={() => setExpanded(!expanded)}
            className="text-xs font-medium text-ink-3 hover:text-logo-primary cursor-pointer"
          >
            {expanded
              ? t("settings.history.showLess")
              : t("settings.history.showMore")}
          </button>
        )}
      </div>
      )}

      <AudioPlayer onLoadRequest={handleLoadAudio} className="w-full" />
    </div>
  );
};
