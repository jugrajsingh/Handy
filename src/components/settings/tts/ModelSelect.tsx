import { useTranslation } from "react-i18next";
import type { TtsModelInfo } from "@/bindings";
import { Dropdown } from "@/components/ui/Dropdown";
import { Button } from "@/components/ui/Button";

export function selectDownloadedModel(
  models: readonly TtsModelInfo[],
  id: string,
  change: (model: TtsModelInfo) => void,
) {
  const next = models.find((row) => row.id === id);
  if (next?.downloaded) change(next);
}

export function ModelSelect({
  models,
  value,
  change,
  openModels,
}: {
  models: readonly TtsModelInfo[];
  value: string;
  change: (model: TtsModelInfo) => void;
  openModels: () => void;
}) {
  const { t } = useTranslation();
  const missing = !models.some((row) => row.id === value && row.downloaded);
  const available = models.filter((row) => row.downloaded);
  const options = available.map((row) => ({
    value: row.id,
    label: row.display_name,
    disabled: false,
  }));
  if (missing)
    options.unshift({
      value,
      label: `${models.find((row) => row.id === value)?.display_name ?? value}: ${t("tts.errors.notDownloaded")}`,
      disabled: true,
    });
  return (
    <div className="flex flex-col items-end gap-2">
      <Dropdown
        options={options}
        selectedValue={value}
        onSelect={(id) => selectDownloadedModel(models, id, change)}
        disabled={available.length === 0}
      />
      {missing && (
        <Button variant="ghost" size="sm" onClick={openModels}>
          {t("tts.openModels")}
        </Button>
      )}
    </div>
  );
}
