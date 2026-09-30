import React, { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { HistoryCompareView, HistoryEntry } from "@/bindings";
import { wordDiff } from "@/lib/utils/wordDiff";
import { cleanedText, copyHistoryRaw } from "@/lib/utils/historyPresentation";
import { Button } from "../../ui/Button";

type CompareEntry = Pick<
  HistoryEntry,
  "transcription_text" | "post_processed_text" | "post_process_model"
>;

export function HistoryCompare({
  entry,
  view,
  onCopyRaw,
}: {
  entry: CompareEntry;
  view: HistoryCompareView;
  onCopyRaw: () => Promise<boolean>;
}): React.JSX.Element {
  const { t } = useTranslation();
  const cleaned = cleanedText(entry);
  const diff = useMemo(
    () =>
      view === "diff" && cleaned !== null
        ? wordDiff(entry.transcription_text, cleaned)
        : null,
    [entry.transcription_text, cleaned, view],
  );
  if (cleaned === null)
    return (
      <p className="text-sm whitespace-pre-wrap break-words select-text">
        {entry.transcription_text}
      </p>
    );
  const effectiveView = diff?.kind === "side_by_side" ? "side_by_side" : view;
  const textClass = "text-sm whitespace-pre-wrap break-words select-text";
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs text-logo-primary">
          {entry.post_process_model
            ? t("settings.history.cleanedWith", {
                model: entry.post_process_model,
              })
            : t("settings.history.cleaned")}
        </span>
        <Button
          variant="secondary"
          size="sm"
          onClick={() =>
            void copyHistoryRaw(onCopyRaw, () =>
              toast.error(t("settings.history.copyError")),
            )
          }
        >
          {t("settings.history.copyRaw")}
        </Button>
      </div>
      {effectiveView === "diff" && diff?.kind === "diff" ? (
        <p className={textClass}>
          {diff.tokens.map((token, index) => (
            <React.Fragment key={index}>
              {index > 0 && " "}
              {token.kind === "removed" ? (
                <del className="text-red-500">{token.text}</del>
              ) : token.kind === "added" ? (
                <mark className="bg-green-500/20 text-text">{token.text}</mark>
              ) : (
                <span>{token.text}</span>
              )}
            </React.Fragment>
          ))}
        </p>
      ) : (
        <div
          className={
            effectiveView === "side_by_side"
              ? "grid grid-cols-1 sm:grid-cols-2 gap-3"
              : "flex flex-col gap-3"
          }
        >
          <div>
            <h3 className="text-xs text-mid-gray">
              {t("settings.history.raw")}
            </h3>
            <p className={textClass}>{entry.transcription_text}</p>
          </div>
          <div>
            <h3 className="text-xs text-mid-gray">
              {t("settings.history.cleaned")}
            </h3>
            <p className={textClass}>{cleaned}</p>
          </div>
        </div>
      )}
    </div>
  );
}
