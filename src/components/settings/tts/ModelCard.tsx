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
}: ModelCardProps) {
  const { t } = useTranslation();
  const row = rowActions(model, state, actions);
  const rating = completeRating(model);
  const invalidNpu =
    model.npu_driver_too_old ||
    (model.preparation.state === "failed" &&
      model.preparation.device === "npu");
  return (
    <article
      data-tts-model={model.id}
      className="w-full rounded-xl border border-mid-gray/20 p-4"
    >
      <h3>{model.display_name}</h3>
      <p>{t(`tts.description.${model.id}`)}</p>
      <p>{t("tts.voiceCount", { count: model.voices.length })}</p>
      <p>{model.license}</p>
      <p>{t(model.attribution)}</p>
      <a href={model.card_url} target="_blank" rel="noreferrer">
        {t("tts.modelCard")}
      </a>
      <p>
        {t("tts.downloadSize", {
          megabytes: (model.size_bytes / 1e6).toFixed(1),
        })}
      </p>
      <RatingBars
        modelId={model.id}
        rating={rating}
        available={available}
        invalidNpu={invalidNpu}
      />
      <RunOnControl
        invalidNpu={invalidNpu}
        downloaded={model.downloaded}
        available={available}
        rating={rating}
        value={model.run_on}
        preparation={model.preparation}
        change={(value) => runOn(model.id, value)}
      />
      {downloadProgress && (
        <progress
          aria-label={t("tts.downloadProgress")}
          max={downloadProgress.total_bytes}
          value={downloadProgress.downloaded_bytes}
        />
      )}
      <div className="flex gap-3">
        <button disabled={!row.canUse} onClick={() => applyModelUse(row)}>
          {t(row.isActive ? "tts.active" : "tts.use")}
        </button>
        {!model.downloaded && (
          <button disabled={!row.canDownload} onClick={row.download}>
            {t("tts.download")}
          </button>
        )}
        {model.downloaded && (
          <button disabled={!row.canDelete} onClick={row.delete}>
            {t("tts.delete")}
          </button>
        )}
        {row.isDownloading && (
          <button onClick={() => cancel(model.id)}>{t("tts.cancel")}</button>
        )}
      </div>
    </article>
  );
}
