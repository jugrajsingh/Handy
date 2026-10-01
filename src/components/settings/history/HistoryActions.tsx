import React from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  Copy,
  ClipboardCopy,
  Star,
  RotateCcw,
  Trash2,
} from "lucide-react";

export interface HistoryActionsProps {
  processed: boolean;
  rawAvailable: boolean;
  hasText: boolean;
  saved: boolean;
  retrying: boolean;
  copied: boolean;
  onCopy: () => void;
  onCopyRaw: () => void;
  onToggleSaved: () => void;
  onRetranscribe: () => void;
  onDelete: () => void;
}

function IconButton({
  onClick,
  title,
  disabled,
  active,
  children,
}: {
  onClick: () => void;
  title: string;
  disabled: boolean;
  active?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${active ? "text-logo-primary hover:text-logo-primary/80" : "text-text/50 hover:text-logo-primary"}`}
    >
      {children}
    </button>
  );
}

export function HistoryActions(props: HistoryActionsProps): React.JSX.Element {
  const { t } = useTranslation();
  return (
    <div className="flex items-center">
      <IconButton
        onClick={props.onCopy}
        disabled={!props.hasText || props.retrying}
        title={t(
          props.processed
            ? "settings.history.copyPostProcessed"
            : "settings.history.copyToClipboard",
        )}
      >
        {props.copied ? (
          <Check width={16} height={16} />
        ) : (
          <Copy width={16} height={16} />
        )}
      </IconButton>
      {props.processed && (
        <IconButton
          onClick={props.onCopyRaw}
          disabled={!props.rawAvailable || props.retrying}
          title={t("settings.history.copyRaw")}
        >
          <ClipboardCopy width={16} height={16} />
        </IconButton>
      )}
      <IconButton
        onClick={props.onToggleSaved}
        disabled={props.retrying}
        active={props.saved}
        title={t(
          props.saved ? "settings.history.unsave" : "settings.history.save",
        )}
      >
        <Star
          width={16}
          height={16}
          fill={props.saved ? "currentColor" : "none"}
        />
      </IconButton>
      <IconButton
        onClick={props.onRetranscribe}
        disabled={props.retrying}
        title={t("settings.history.retranscribe")}
      >
        <RotateCcw
          width={16}
          height={16}
          className={props.retrying ? "animate-spin" : ""}
        />
      </IconButton>
      <IconButton
        onClick={props.onDelete}
        disabled={props.retrying}
        title={t("settings.history.delete")}
      >
        <Trash2 width={16} height={16} />
      </IconButton>
    </div>
  );
}
