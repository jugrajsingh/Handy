import { useEffect, useState, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { commands, type Selection, type TtsModelInfo } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { unwrapTts } from "./ModelsPage";
import { selectionEntries } from "./actions";
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
  const id = selected.model_id;
  const model = models.find((row) => row.id === id);
  const change = (patch: Partial<Selection>) =>
    void choose.edit(patch, selected);
  return (
    <section className="w-full max-w-3xl flex flex-col gap-4">
      <h2>{t("tts.settingsTitle")}</h2>
      {error && <p role="alert">{t(error)}</p>}
      <label>
        {t("tts.model")}
        <select
          value={id}
          onChange={(event) => {
            const next = models.find((row) => row.id === event.target.value);
            if (next)
              change({
                model_id: next.id,
                voice: settings?.tts_voice?.[next.id] ?? next.default_voice,
              });
          }}
        >
          {models
            .filter((row) => row.downloaded)
            .map((row) => (
              <option key={row.id} value={row.id}>
                {row.display_name}
              </option>
            ))}
        </select>
      </label>
      <label>
        {t("tts.voice")}
        <select
          value={selected.voice}
          onChange={(event) => change({ voice: event.target.value })}
        >
          {model?.voices.map((voice) => (
            <option key={voice} value={voice}>
              {voice}
            </option>
          ))}
        </select>
      </label>
      <button
        disabled={!model?.downloaded}
        onClick={() =>
          void commands
            .previewTtsVoice(selected.voice, t("tts.previewSentence"))
            .then(unwrapTts)
            .catch(() => setError("tts.errors.generic"))
        }
      >
        {t("tts.preview")}
      </button>
      <label>
        {t("tts.speed", { value: selected.speed })}
        <input
          type="range"
          min={0.5}
          max={2}
          step={0.1}
          value={selected.speed}
          onChange={(event) => change({ speed: Number(event.target.value) })}
        />
      </label>
      <label>
        {t("tts.pitch", { value: selected.pitch })}
        <input
          type="range"
          min={-6}
          max={6}
          step={1}
          value={selected.pitch}
          onChange={(event) => change({ pitch: Number(event.target.value) })}
        />
      </label>
      <label>
        {t("tts.output")}
        <select
          value={selected.output_device ?? ""}
          onChange={(event) =>
            change({ output_device: event.target.value || null })
          }
        >
          <option value="">{t("tts.sameAsHandy")}</option>
          {outputDevices.map((device) => (
            <option key={device.index} value={device.name}>
              {device.name}
            </option>
          ))}
        </select>
      </label>
      <label>
        <input
          type="checkbox"
          checked={selected.read_code}
          onChange={(event) => change({ read_code: event.target.checked })}
        />
        {t("tts.readCode")}
      </label>
    </section>
  );
}
