import { useTranslation } from "react-i18next";
import {
  barFill,
  deviceRows,
  type ModelRating,
  type TtsDevice,
} from "./presentation";
export interface RatingBarsProps {
  modelId: string;
  rating: ModelRating;
  available: readonly TtsDevice[];
  invalidNpu: boolean;
}
export function RatingBars({
  modelId,
  rating,
  available,
  invalidNpu,
}: RatingBarsProps) {
  const { t } = useTranslation();
  return (
    <div data-model-ratings={modelId}>
      <div data-rating="quality">
        <span>{t("tts.ratings.quality")}</span>
        <meter
          min={0}
          max={100}
          value={barFill(rating.quality)}
          aria-label={t("tts.ratings.quality")}
        />
      </div>
      {deviceRows(rating, available, invalidNpu).map((row) => (
        <div
          key={row.device}
          data-device={row.device}
          title={
            row.supported
              ? t("tts.ratings.tooltip", row.tooltip)
              : t(row.stateKey ?? "tts.ratings.notSupported")
          }
        >
          <span>{t(`tts.devices.${row.device}`)}</span>
          {row.supported ? (
            <meter
              min={0}
              max={100}
              value={row.fill}
              aria-label={t("tts.ratings.speed")}
            />
          ) : (
            <span>{t(row.stateKey ?? "tts.ratings.notSupported")}</span>
          )}
          {row.recommended && (
            <span data-recommended="true">{t("tts.ratings.recommended")}</span>
          )}
        </div>
      ))}
    </div>
  );
}
