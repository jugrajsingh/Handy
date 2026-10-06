import { useTranslation } from "react-i18next";
import {
  runOnChoices,
  type ModelRating,
  type TtsDevice,
  type TtsRunOn,
} from "./presentation";
export type PreparationState =
  | { state: "idle" }
  | {
      state: "preparing";
      device: TtsDevice;
      completed: number;
      total: number;
      graph: string;
    }
  | { state: "ready"; device: TtsDevice }
  | { state: "failed"; device: TtsDevice; error: string };
export interface RunOnControlProps {
  downloaded: boolean;
  invalidNpu: boolean;
  available: readonly TtsDevice[];
  rating: ModelRating;
  value: TtsRunOn;
  preparation: PreparationState;
  change: (value: TtsRunOn) => void;
}
export function applyRunOnChoice(
  inputs: Pick<
    RunOnControlProps,
    "downloaded" | "invalidNpu" | "available" | "rating" | "change"
  >,
  next: string,
): boolean {
  const choices = runOnChoices(inputs.available);
  if (!inputs.downloaded || !choices.includes(next as TtsRunOn)) return false;
  if (next === "npu" && inputs.invalidNpu) return false;
  if (next !== "auto" && !inputs.rating.devices[next as TtsDevice].supported)
    return false;
  inputs.change(next as TtsRunOn);
  return true;
}
export function RunOnControl({
  downloaded,
  invalidNpu,
  available,
  rating,
  value,
  preparation,
  change,
}: RunOnControlProps) {
  const { t } = useTranslation();
  const choices = runOnChoices(available);
  return (
    <div>
      <label>
        {t("tts.runOn")}
        <select
          value={value}
          disabled={!downloaded}
          onChange={(event) => {
            applyRunOnChoice(
              { downloaded, invalidNpu, available, rating, change },
              event.target.value,
            );
          }}
        >
          {choices.map((device) => (
            <option
              key={device}
              value={device}
              disabled={
                device !== "auto" &&
                (!rating.devices[device].supported ||
                  (device === "npu" && invalidNpu))
              }
            >
              {t(`tts.devices.${device}`)}
            </option>
          ))}
        </select>
      </label>
      {preparation.state === "preparing" && (
        <div role="status">
          <span>
            {t("tts.preparing", {
              device: t(`tts.devices.${preparation.device}`),
              graph: preparation.graph,
            })}
          </span>
          <progress
            max={Math.max(1, preparation.total)}
            value={Math.max(
              0,
              Math.min(preparation.total, preparation.completed),
            )}
          />
        </div>
      )}
      {preparation.state === "failed" && (
        <span role="alert">{t(preparation.error)}</span>
      )}
    </div>
  );
}
