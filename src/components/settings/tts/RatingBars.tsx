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
function Bar({ fill, label }: { fill: number; label: string }) {
  return (
    <div
      className="w-16 h-1.5 bg-mid-gray/20 rounded-full overflow-hidden"
      role="meter"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={fill}
      aria-label={label}
    >
      <div
        className="h-full bg-logo-primary rounded-full"
        style={{ width: `${fill}%` }}
      />
    </div>
  );
}
export function RatingBars({
  modelId,
  rating,
  available,
  invalidNpu,
}: RatingBarsProps) {
  const { t } = useTranslation();
  return (
    <div
      data-model-ratings={modelId}
      className="flex items-center ms-4 shrink-0"
    >
      <div className="space-y-1">
        <div data-rating="quality" className="flex items-center gap-2">
          <p className="text-xs text-text/60 w-24 text-end">
            {t("tts.ratings.quality")}
          </p>
          <Bar
            fill={barFill(rating.quality)}
            label={t("tts.ratings.quality")}
          />
        </div>
        {deviceRows(rating, available, invalidNpu).map((row) => (
          <div
            key={row.device}
            data-device={row.device}
            className="flex items-center gap-2"
            title={t("tts.ratings.tooltip", row.tooltip)}
          >
            <p className="text-xs text-text/60 w-24 text-end">
              {t(`tts.devices.${row.device}`)}
              {row.recommended && (
                <span
                  data-recommended="true"
                  role="img"
                  aria-label={t("tts.ratings.recommended")}
                  className="ms-1 text-logo-primary"
                >
                  {"\u2022"}
                </span>
              )}
            </p>
            <Bar fill={row.fill} label={t("tts.ratings.speed")} />
          </div>
        ))}
      </div>
    </div>
  );
}
