import { useEffect, useState, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { commands, type Selection, type TtsModelInfo } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { unwrapTts } from "./ModelsPage";
import { previewVoice, selectionEntries } from "./actions";
import { ModelSelect } from "./ModelSelect";
import { Alert } from "@/components/ui/Alert";
import { Button } from "@/components/ui/Button";
import { Dropdown } from "@/components/ui/Dropdown";
import { SettingContainer } from "@/components/ui/SettingContainer";
import { SettingsGroup } from "@/components/ui/SettingsGroup";
import { Slider } from "@/components/ui/Slider";
import { ToggleSwitch } from "@/components/ui/ToggleSwitch";

export function SettingsPage() {
  const { t } = useTranslation();
  const {
    settings,
    outputDevices,
    refreshSettingsChecked,
    refreshOutputDevices,
  } = useSettings();
  const [models, setModels] = useState<TtsModelInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Selection | null>(null);
  useEffect(() => {
    let alive = true;
    void commands
      .getTtsModels()
      .then(unwrapTts)
      .then((value) => {
        if (alive) setModels(value);
      })
      .catch(() => {
        if (alive) setError("tts.errors.generic");
      });
    void refreshOutputDevices();
    let unlisten: (() => void) | undefined;
    void listen<string>("tts-preview-error", ({ payload }) => {
      if (alive) setError(payload);
    }).then((stop) => {
      if (alive) unlisten = stop;
      else stop();
    });
    return () => {
      alive = false;
      unlisten?.();
    };
  }, [refreshOutputDevices]);
  const choose = useMemo(
    () =>
      selectionEntries({
        select: async (value) => {
          unwrapTts(await commands.setTtsSelection(value));
        },
        refresh: refreshSettingsChecked,
        error: setError,
        clearError: () => setError(null),
        display: setDraft,
      }),
    [refreshSettingsChecked],
  );
  const persisted = useMemo<Selection | null>(() => {
    if (!settings) return null;
    const id = settings.tts_model_id ?? "pocket-en";
    const model = models.find((row) => row.id === id);
    return {
      model_id: id,
      voice: settings.tts_voice?.[id] ?? model?.default_voice ?? "alba",
      speed: settings.tts_speed ?? 1,
      pitch: settings.tts_pitch_semitones ?? 0,
      output_device: settings.tts_output_device ?? null,
      read_code: settings.tts_read_code_blocks ?? false,
    };
  }, [settings, models]);
  useEffect(() => {
    if (persisted) choose.sync(persisted);
  }, [choose, persisted]);
  if (!persisted) return null;
  const selected = draft ?? persisted;
  const change = (patch: Partial<Selection>) =>
    void choose.edit(patch, selected);
  return (
    <SettingsControls
      selected={selected}
      models={models}
      outputDevices={outputDevices}
      error={error}
      change={change}
      openModels={() => void emit("open-tts-models")}
      preview={() =>
        void previewVoice(t, selected.voice, (voice, text) =>
          commands.previewTtsVoice(voice, text),
        )
          .then(unwrapTts)
          .catch(() => setError("tts.errors.generic"))
      }
      rememberedVoices={settings?.tts_voice}
    />
  );
}

export function SettingsControls({
  selected,
  models,
  outputDevices,
  error,
  change,
  preview,
  openModels,
  rememberedVoices,
}: {
  selected: Selection;
  models: readonly TtsModelInfo[];
  outputDevices: readonly { name: string; index: string | number }[];
  error: string | null;
  change: (patch: Partial<Selection>) => void;
  preview: () => void;
  openModels: () => void;
  rememberedVoices?: Partial<Record<string, string>>;
}) {
  const { t } = useTranslation();
  const model = models.find((row) => row.id === selected.model_id);
  return (
    <section className="max-w-3xl w-full mx-auto space-y-6">
      {error && (
        <div role="alert">
          <Alert>{t(error)}</Alert>
        </div>
      )}
      <SettingsGroup title={t("tts.settingsTitle")}>
        <SettingContainer
          title={t("tts.model")}
          description={t("tts.help.model")}
          grouped
        >
          <ModelSelect
            models={models}
            value={selected.model_id}
            change={(next) =>
              change({
                model_id: next.id,
                voice: rememberedVoices?.[next.id] ?? next.default_voice,
              })
            }
            openModels={openModels}
          />
        </SettingContainer>
        <SettingContainer
          title={t("tts.voice")}
          description={t("tts.help.voice")}
          grouped
        >
          <div className="flex items-center gap-2">
            <Dropdown
              options={(model?.voices ?? []).map((voice) => ({
                value: voice,
                label: voice,
              }))}
              selectedValue={selected.voice}
              placeholder={selected.voice}
              onSelect={(voice) => change({ voice })}
              disabled={!model?.voices.length}
            />
            <Button
              variant="secondary"
              onClick={preview}
              disabled={!model?.downloaded}
            >
              {t("tts.preview")}
            </Button>
          </div>
        </SettingContainer>
        <Slider
          label={t("tts.speedLabel")}
          description={t("tts.help.speed")}
          grouped
          min={0.5}
          max={2}
          step={0.1}
          value={selected.speed}
          formatValue={(value) => `${value.toFixed(1)}x`}
          onChange={(speed) => change({ speed })}
        />
        <Slider
          label={t("tts.pitchLabel")}
          description={t("tts.help.pitch")}
          grouped
          min={-6}
          max={6}
          step={1}
          value={selected.pitch}
          formatValue={(value) => value.toFixed(0)}
          onChange={(pitch) => change({ pitch })}
        />
        <SettingContainer
          title={t("tts.output")}
          description={t("tts.help.output")}
          grouped
        >
          <Dropdown
            options={[
              { value: "", label: t("tts.sameAsHandy") },
              ...outputDevices.map((device) => ({
                value: device.name,
                label: device.name,
              })),
            ]}
            selectedValue={selected.output_device ?? ""}
            onSelect={(value) => change({ output_device: value || null })}
          />
        </SettingContainer>
        <ToggleSwitch
          label={t("tts.readCode")}
          description={t("tts.help.readCode")}
          grouped
          checked={selected.read_code}
          onChange={(read_code) => change({ read_code })}
        />
      </SettingsGroup>
    </section>
  );
}
