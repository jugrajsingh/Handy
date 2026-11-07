import { useEffect, useLayoutEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { ClipboardPaste, Pause, Play, Square, X } from "lucide-react";
import type { SessionSnapshot } from "@/bindings";
import { Alert } from "@/components/ui/Alert";
import { Button } from "@/components/ui/Button";
import { Textarea } from "@/components/ui/Textarea";
import { routePopupKey } from "./actions";
import {
  highlightedText,
  progressPresentation,
  popupMode,
  formatTime,
  registerPopupPaste,
  restorePopupEditor,
} from "./presentation";
export interface PopupProps {
  text: string;
  snapshot: SessionSnapshot;
  modelName?: string;
  editing: boolean;
  enterEdit: (text: string) => void;
  edit: (text: string) => void;
  toggle: () => void;
  stop: () => void;
  close: () => void;
  readClipboard: () => void;
  models: () => void;
}
export function ReadAloudPopup({
  text,
  snapshot,
  modelName,
  editing,
  enterEdit,
  edit,
  toggle,
  stop,
  close,
  readClipboard,
  models,
}: PopupProps) {
  const { t } = useTranslation();
  const mark = useRef<HTMLElement>(null);
  const preview = useRef<HTMLDivElement>(null);
  const editor = useRef<HTMLTextAreaElement | null>(null);
  const caret = useRef<{ start: number; end: number } | null>(null);
  const parts = highlightedText(
    text,
    snapshot.source_start,
    snapshot.source_end,
  );
  const progress = progressPresentation(snapshot);
  const mode = popupMode(snapshot.state, editing, snapshot.error);
  const playing =
    snapshot.state === "playing" || snapshot.state === "buffering";
  useEffect(
    () => mark.current?.scrollIntoView({ block: "nearest" }),
    [snapshot.source_start, snapshot.source_end, mode],
  );
  useLayoutEffect(() => {
    if (mode !== "editing" || !caret.current || !editor.current) return;
    restorePopupEditor(editor.current, caret.current);
    caret.current = null;
  });
  const textSelection = () => {
    const selection = window.getSelection();
    const root = preview.current;
    if (!selection?.rangeCount || !root) return null;
    const selected = selection.getRangeAt(0);
    if (
      !root.contains(selected.startContainer) ||
      !root.contains(selected.endContainer)
    )
      return null;
    const prefix = selected.cloneRange();
    prefix.selectNodeContents(root);
    prefix.setEnd(selected.startContainer, selected.startOffset);
    const start = prefix.toString().length;
    return { start, end: start + selected.toString().length };
  };
  useEffect(() =>
    registerPopupPaste(window, {
      isEditorTarget: (target) => target === editor.current,
      getText: () => text,
      getSelection: () =>
        mode === "reading"
          ? textSelection()
          : editor.current
            ? {
                start: editor.current.selectionStart,
                end: editor.current.selectionEnd,
              }
            : null,
      enterEdit: (next, position) => {
        caret.current = { start: position, end: position };
        enterEdit(next);
      },
    }),
  );
  return (
    <section
      className="h-screen flex flex-col overflow-hidden rounded-xl border border-line bg-background text-text text-sm"
      aria-label={t("tts.title")}
      onKeyDown={(event) => routePopupKey(event, { close, toggle })}
    >
      <header className="flex shrink-0 items-center justify-between gap-2 border-b border-line px-3 py-1">
        <span className="truncate font-medium">{modelName}</span>
        <Button
          variant="ghost"
          size="sm"
          onClick={close}
          aria-label={t("tts.close")}
        >
          <X className="h-4 w-4" aria-hidden="true" />
        </Button>
      </header>
      <div className="min-h-0 flex-1 p-2">
        {mode === "reading" ? (
          <div
            ref={preview}
            className="h-full overflow-auto whitespace-pre-wrap break-words rounded-md px-3 py-2 cursor-text"
            data-source-preview
            onClick={() => {
              caret.current = textSelection() ?? {
                start: text.length,
                end: text.length,
              };
              enterEdit(text);
            }}
          >
            <span className="text-ink-3">{parts.before}</span>
            <mark ref={mark} className="rounded bg-logo-primary/25 text-text">
              {parts.current}
            </mark>
            {parts.after}
          </div>
        ) : (
          <div
            className="h-full"
            ref={(node) => {
              editor.current = node?.querySelector("textarea") ?? null;
            }}
          >
            <Textarea
              aria-label={t("tts.textLabel")}
              placeholder={t("tts.placeholder")}
              variant="compact"
              className="w-full h-full min-h-0! resize-none! font-normal"
              value={text}
              onChange={(event) => edit(event.target.value)}
            />
          </div>
        )}
      </div>
      {snapshot.error && (
        <div role="alert" className="shrink-0 px-2 pb-1">
          <Alert className="p-2">
            {t(snapshot.error)}
            {snapshot.error === "tts.errors.notDownloaded" && (
              <Button variant="ghost" size="sm" onClick={models}>
                {t("tts.openModels")}
              </Button>
            )}
          </Alert>
        </div>
      )}
      {snapshot.output_fallback && (
        <span className="shrink-0 px-3 text-xs text-ink-3" data-output-fallback>
          {t("tts.outputFallback")}
        </span>
      )}
      <div className="flex shrink-0 items-center gap-2 border-t border-line px-3 py-2">
        <Button
          variant="secondary"
          size="sm"
          onClick={toggle}
          disabled={!text.trim() || snapshot.state === "starting"}
          aria-label={t(playing ? "tts.pause" : "tts.play")}
        >
          {playing ? (
            <Pause className="h-4 w-4" aria-hidden="true" />
          ) : (
            <Play className="h-4 w-4" aria-hidden="true" />
          )}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          onClick={readClipboard}
          aria-label={t("tts.readClipboard")}
        >
          <ClipboardPaste className="h-4 w-4" aria-hidden="true" />
        </Button>
        <Button
          variant="ghost"
          size="sm"
          onClick={stop}
          disabled={["waiting", "finished"].includes(snapshot.state)}
          aria-label={t("tts.stop")}
        >
          <Square className="h-4 w-4" aria-hidden="true" />
        </Button>
        <div
          role="progressbar"
          aria-label={t("tts.progress")}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={progress.percent}
          className="h-1.5 min-w-8 flex-1 overflow-hidden rounded-full bg-line"
        >
          <div
            className="h-full bg-logo-primary"
            style={{ width: `${progress.percent}%` }}
          />
        </div>
        <span className="tabular-nums text-xs whitespace-nowrap">
          {formatTime(snapshot.elapsed_seconds)} /{" "}
          {formatTime(snapshot.elapsed_seconds + snapshot.remaining_seconds)}
        </span>
        <span role="status" className="text-xs text-ink-3">
          {t(progress.stateKey)}
        </span>
      </div>
    </section>
  );
}
