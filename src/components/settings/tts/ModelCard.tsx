import type { ReactNode } from "react";
import {
  AudioLines,
  Check,
  Download,
  HardDrive,
  Info,
  KeyRound,
  Loader2,
  Trash2,
} from "lucide-react";
import Badge from "@/components/ui/Badge";
import { Button } from "@/components/ui/Button";
import { formatModelSize } from "@/lib/utils/format";
import { useTranslation } from "react-i18next";
import type { TtsModelInfo, Device, RunOn } from "@/bindings";
import { RatingBars } from "./RatingBars";
import { RunOnControl } from "./RunOnControl";
import { rowActions, type TtsRowState, type TtsRowCallbacks } from "./actions";
import type { ModelRating, DeviceRating } from "./presentation";
export function completeRating(model: TtsModelInfo): ModelRating {
  const { cpu, gpu, npu } = model.rating.devices;
  if (!cpu || !gpu || !npu) throw new Error("tts.errors.invalidRatings");
  const adapt = (row: typeof cpu): DeviceRating => {
    const placement: Record<string, Device> = {};
    for (const [name, device] of Object.entries(row.placement)) {
      if (device !== undefined) placement[name] = device;
    }
    return { ...row, placement };
  };
  return {
    quality: model.rating.quality,
    devices: { cpu: adapt(cpu), gpu: adapt(gpu), npu: adapt(npu) },
  };
}
export interface DownloadProgress {
  model_id: string;
  downloaded_bytes: number;
  total_bytes: number;
}
export interface ModelCardProps {
  downloadProgress?: DownloadProgress;
  model: TtsModelInfo;
  available: readonly Device[];
  state: TtsRowState;
  actions: TtsRowCallbacks;
  runOn: (id: string, value: RunOn) => void;
  cancel: (id: string) => void;
  tokenSet?: boolean;
  setToken?: () => void;
  tokenEntry?: ReactNode;
  downloadError?: string;
}
export function applyModelUse(
  row: Pick<ReturnType<typeof rowActions>, "canUse" | "use">,
): void {
  if (row.canUse) row.use();
}
export function ModelCard({
  model,
  downloadProgress,
  available,
  state,
  actions,
  runOn,
  cancel,
  tokenSet = false,
  setToken,
  tokenEntry,
  downloadError,
}: ModelCardProps) {
  const { t } = useTranslation();
  const row = rowActions(model, state, actions, tokenSet);
  const rating = completeRating(model);
  const invalidNpu =
    model.npu_driver_too_old ||
    (model.preparation.state === "failed" &&
      model.preparation.device === "npu");
  const needsToken = model.id === "pocket-en" && !model.downloaded;
  return (
    <article
      data-tts-model={model.id}
      className={`flex flex-col rounded-xl px-4 py-3 gap-2 text-left transition-all duration-200 border-2 ${row.isActive ? "border-logo-primary/50 bg-logo-primary/10" : "border-mid-gray/20"}`}
    >
      <div className="flex justify-between items-center w-full">
        <div className="flex flex-col items-start flex-1 min-w-0">
          <div className="flex items-center gap-3 flex-wrap">
            <h3 className="text-base font-semibold text-text">
              {model.display_name}
            </h3>
            {row.isActive && (
              <span data-badge="active">
                <Badge variant="primary">
                  <Check className="w-3 h-3 mr-1" />
                  {t("tts.active")}
                </Badge>
              </span>
            )}
            {!row.isActive && model.id === "pocket-en" && (
              <span data-badge="recommended">
                <Badge variant="primary">{t("tts.recommended")}</Badge>
              </span>
            )}
            {row.isDownloading && (
              <span data-badge="downloading">
                <Badge variant="secondary">
                  <Loader2 className="w-3 h-3 mr-1 animate-spin" />
                  {t("tts.downloading")}
                </Badge>
              </span>
            )}
          </div>
          <p className="text-text/60 text-sm leading-relaxed">
            {t(`tts.description.${model.id}`)}
          </p>
        </div>
        <RatingBars
          modelId={model.id}
          rating={rating}
          available={available}
          invalidNpu={invalidNpu}
        />
      </div>
      <hr className="w-full border-mid-gray/20" />
      <div className="flex items-center gap-3 w-full -mb-0.5 mt-0.5 flex-wrap">
        <div className="flex items-center gap-1 text-xs text-text/50">
          <AudioLines className="w-3.5 h-3.5" />
          <span>{t("tts.voiceCount", { count: model.voices.length })}</span>
        </div>
        <a
          href={model.card_url}
          target="_blank"
          rel="noreferrer"
          title={t(model.attribution)}
          className="flex items-center gap-1 text-xs text-text/50 hover:text-logo-primary"
        >
          {model.license}
          <Info className="w-3.5 h-3.5" />
        </a>
        <div className="flex items-center gap-3 ms-auto">
          {model.downloaded && (
            <RunOnControl
              invalidNpu={invalidNpu}
              downloaded={model.downloaded}
              available={available}
              rating={rating}
              value={model.run_on}
              preparation={model.preparation}
              change={(value) => runOn(model.id, value)}
            />
          )}
          {model.downloaded ? (
            <span className="flex items-center gap-1.5 text-xs text-text/50">
              <HardDrive className="w-3.5 h-3.5" />
              {formatModelSize(model.size_bytes / 1e6)}
            </span>
          ) : (
            <Button
              variant="ghost"
              size="sm"
              className="flex items-center gap-1.5 text-text/50"
              disabled={!row.canDownload}
              onClick={row.download}
            >
              <Download className="w-3.5 h-3.5" />
              <span className="sr-only">{t("tts.download")}</span>
              <span>{formatModelSize(model.size_bytes / 1e6)}</span>
            </Button>
          )}
          {model.downloaded && !row.isActive && (
            <Button
              variant="primary-soft"
              size="sm"
              disabled={!row.canUse}
              onClick={() => applyModelUse(row)}
            >
              {t("tts.use")}
            </Button>
          )}
          {model.downloaded && (
            <Button
              variant="ghost"
              size="sm"
              disabled={!row.canDelete}
              onClick={row.delete}
              className="flex items-center gap-1.5 text-logo-primary/85 hover:text-logo-primary hover:bg-logo-primary/10"
            >
              <Trash2 className="w-3.5 h-3.5" />
              <span>{t("tts.delete")}</span>
            </Button>
          )}
          {row.isDownloading && (
            <Button
              variant="danger-ghost"
              size="sm"
              onClick={() => cancel(model.id)}
            >
              {t("tts.cancel")}
            </Button>
          )}
        </div>
        {needsToken && (
          <div
            className="flex items-center gap-1.5 text-xs text-text/50 w-full"
            data-token-chip={tokenSet ? "set" : "required"}
          >
            <KeyRound className="w-3.5 h-3.5" />
            <span>{t(tokenSet ? "tts.tokenSet" : "tts.needsToken")}</span>
            <Button variant="ghost" size="sm" onClick={setToken}>
              {t("tts.saveToken")}
            </Button>
          </div>
        )}
        {needsToken && tokenEntry}
        {downloadError && (
          <p role="alert" className="w-full text-sm text-logo-primary">
            {t(downloadError)}
          </p>
        )}
        {downloadProgress && row.isDownloading && (
          <progress
            className="w-full h-1.5 accent-logo-primary"
            aria-label={t("tts.downloadProgress")}
            max={downloadProgress.total_bytes}
            value={downloadProgress.downloaded_bytes}
          />
        )}
      </div>
    </article>
  );
}
