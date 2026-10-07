import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import type { SessionSnapshot } from "@/bindings";
import { routePopupKey } from "./actions";
import { highlightedText, progressPresentation } from "./presentation";
export interface PopupProps {
  text: string;
  snapshot: SessionSnapshot;
  edit: (text: string) => void;
  toggle: () => void;
  stop: () => void;
  close: () => void;
  models: () => void;
}
export function ReadAloudPopup({
  text,
  snapshot,
  edit,
  toggle,
  stop,
  close,
  models,
}: PopupProps) {
  const { t } = useTranslation();
  const mark = useRef<HTMLElement>(null);
  const parts = highlightedText(
    text,
    snapshot.source_start,
    snapshot.source_end,
  );
  const progress = progressPresentation(snapshot);
  const playing =
    snapshot.state === "playing" || snapshot.state === "buffering";
  useEffect(
    () => mark.current?.scrollIntoView({ block: "nearest" }),
    [snapshot.source_start, snapshot.source_end],
  );
  return (
    <section
      className="h-screen rounded-xl bg-background p-3 text-text"
      aria-label={t("tts.title")}
      onKeyDown={(event) => routePopupKey(event, { close, toggle })}
    >
      <textarea
        aria-label={t("tts.textLabel")}
        placeholder={t("tts.placeholder")}
        className="w-full h-16 resize-none"
        value={text}
        onChange={(event) => edit(event.target.value)}
      />
      <div className="h-10 overflow-auto" data-source-preview>
        {parts.before}
        <mark ref={mark}>{parts.current}</mark>
        {parts.after}
      </div>
      <div className="flex items-center gap-3">
        <button
          onClick={toggle}
          disabled={!text.trim() || snapshot.state === "starting"}
        >
          {t(playing ? "tts.pause" : "tts.play")}
        </button>
        <button
          onClick={stop}
          disabled={["waiting", "finished"].includes(snapshot.state)}
        >
          {t("tts.stop")}
        </button>
        <progress
          max={100}
          value={progress.percent}
          aria-label={t("tts.progress")}
        />
        <span role="status">
          {t(progress.stateKey)}{" "}
          {t("tts.time", {
            elapsed: progress.elapsed,
            remaining: progress.remaining,
          })}
        </span>
        <button onClick={close} aria-label={t("tts.close")}>
          {t("tts.close")}
        </button>
      </div>
      {snapshot.error && (
        <div role="alert">
          {t(snapshot.error)}
          {snapshot.error === "tts.errors.notDownloaded" && (
            <button onClick={models}>{t("tts.openModels")}</button>
          )}
        </div>
      )}
      {snapshot.output_fallback && <span>{t("tts.outputFallback")}</span>}
    </section>
  );
}
