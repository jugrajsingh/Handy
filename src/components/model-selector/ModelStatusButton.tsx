import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

type ModelStatus =
  | "ready"
  | "loading"
  | "downloading"
  | "verifying"
  | "error"
  | "unloaded"
  | "none";

interface ModelStatusButtonProps {
  status: ModelStatus;
  displayText: string;
  isDropdownOpen: boolean;
  /** Ready, and transcribing in the background. */
  busy?: boolean;
  onClick: () => void;
  className?: string;
  icon?: React.ReactNode;
}

export function modelStatusTitle(
  name: { scrollWidth: number; clientWidth: number } | null,
  fullTitle: string,
): string | undefined {
  return name && name.scrollWidth > name.clientWidth ? fullTitle : undefined;
}

const ModelStatusButton: React.FC<ModelStatusButtonProps> = ({
  status,
  displayText,
  isDropdownOpen,
  busy = false,
  onClick,
  className = "",
  icon,
}) => {
  const { t } = useTranslation();
  const nameRef = useRef<HTMLSpanElement>(null);
  const [title, setTitle] = useState<string | undefined>();
  const fullTitle = t("modelSelector.status", { modelName: displayText });
  useEffect(() => {
    const name = nameRef.current;
    if (!name) return;
    const measure = () => setTitle(modelStatusTitle(name, fullTitle));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(name);
    return () => observer.disconnect();
  }, [displayText, fullTitle]);
  const getStatusColor = (status: ModelStatus): string => {
    switch (status) {
      case "ready":
        return busy
          ? "bg-moss animate-pulse motion-reduce:animate-none"
          : "bg-moss";
      case "loading":
        return "bg-warning animate-pulse";
      case "downloading":
        return "bg-logo-primary animate-pulse";
      case "verifying":
        return "bg-background-ui animate-pulse";
      case "error":
        return "bg-error";
      case "unloaded":
        return "bg-ink-3";
      case "none":
        return "bg-error";
      default:
        return "bg-ink-3";
    }
  };

  return (
    <button
      onClick={onClick}
      className={`flex w-full min-w-0 items-center gap-2 hover:text-text transition-colors ${className}`}
      title={title}
    >
      {icon && (
        <span aria-hidden className="shrink-0">
          {icon}
        </span>
      )}
      <div
        className={`w-2 h-2 shrink-0 rounded-full ${getStatusColor(status)}`}
      />
      <span ref={nameRef} className="flex-1 min-w-0 truncate text-start">
        {displayText}
      </span>
      <svg
        className={`w-3 h-3 shrink-0 transition-transform ${isDropdownOpen ? "rotate-180" : ""}`}
        fill="none"
        stroke="currentColor"
        viewBox="0 0 24 24"
      >
        <path
          strokeLinecap="round"
          strokeLinejoin="round"
          strokeWidth={2}
          d="M19 9l-7 7-7-7"
        />
      </svg>
    </button>
  );
};

export default ModelStatusButton;
