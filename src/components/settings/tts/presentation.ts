export type TtsDevice = "cpu" | "gpu" | "npu";
export type TtsRunOn = "auto" | TtsDevice;
export interface DeviceRating {
  supported: boolean;
  placement: Record<string, TtsDevice>;
  first_audio_p95_s: number | null;
  rtf_1x: number | null;
  rtf_2x: number | null;
  load_s_cold: number | null;
  load_s_cached: number | null;
  speed_score: number | null;
  recommended: boolean;
}
export interface ModelRating {
  quality: number;
  devices: Record<TtsDevice, DeviceRating>;
}
export interface DeviceRow {
  device: TtsDevice;
  fill: number;
  supported: boolean;
  recommended: boolean;
  stateKey: string | null;
  tooltip: Record<string, number | string>;
}
export function barFill(score: number | null): number {
  return score !== null && Number.isFinite(score)
    ? Math.max(0, Math.min(100, score * 10))
    : 0;
}
export function deviceRows(
  rating: ModelRating,
  available: readonly TtsDevice[],
  invalidNpu: boolean,
): DeviceRow[] {
  return (["cpu", "gpu", "npu"] as const)
    .filter((device) => available.includes(device))
    .map((device): DeviceRow => {
      const row = rating.devices[device];
      const supported = row.supported && !(device === "npu" && invalidNpu);
      return {
        device,
        fill: supported ? barFill(row.speed_score) : 0,
        supported,
        recommended: supported && row.recommended,
        stateKey: supported
          ? null
          : device === "npu" && invalidNpu
            ? "tts.errors.npuDriverTooOld"
            : "tts.ratings.notSupported",
        tooltip: supported
          ? {
              firstAudio: row.first_audio_p95_s ?? 0,
              faster:
                row.rtf_1x !== null && row.rtf_1x > 0 ? 1 / row.rtf_1x : 0,
              rtf2x: row.rtf_2x ?? 0,
              cold: row.load_s_cold ?? 0,
              cached: row.load_s_cached ?? 0,
              placement: Object.entries(row.placement)
                .map(([graph, target]) => `${graph}: ${target.toUpperCase()}`)
                .join(", "),
            }
          : {},
      };
    });
}
export function runOnChoices(available: readonly TtsDevice[]): TtsRunOn[] {
  return ["auto", ...available];
}
