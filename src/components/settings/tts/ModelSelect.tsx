import { useTranslation } from "react-i18next";
import type { TtsModelInfo } from "@/bindings";
export function ModelSelect({
  models,
  value,
  change,
}: {
  models: readonly TtsModelInfo[];
  value: string;
  change: (model: TtsModelInfo) => void;
}) {
  const { t } = useTranslation();
  const missing = !models.some((row) => row.id === value && row.downloaded);
  return (
    <label>
      {t("tts.model")}
      <select
        value={value}
        onChange={(event) => {
          const next = models.find((row) => row.id === event.target.value);
          if (next?.downloaded) change(next);
        }}
      >
        {missing && (
          <option value={value} disabled>
            {models.find((row) => row.id === value)?.display_name ?? value}:{" "}
            {t("tts.errors.notDownloaded")}
          </option>
        )}
        {models
          .filter((row) => row.downloaded)
          .map((row) => (
            <option key={row.id} value={row.id}>
              {row.display_name}
            </option>
          ))}
      </select>
    </label>
  );
}
