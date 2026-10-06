import { useEffect, useState, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
import {
  commands,
  type Device,
  type TtsModelInfo,
  type Result,
} from "@/bindings";
import { listen } from "@tauri-apps/api/event";
import { ModelCard, type DownloadProgress } from "./ModelCard";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";
import { downloadController } from "./actions";
export function unwrapTts<T>(result: Result<T, string>): T {
  if (result.status === "error") throw new Error(result.error);
  return result.data;
}
const downloads = downloadController({
  query: async () =>
    unwrapTts(await commands.getTtsModels())
      .filter((model) => model.downloading)
      .map((model) => model.id),
  watch: async (listener) => {
    await listen<DownloadProgress>("tts-download-progress", ({ payload }) =>
      listener(payload),
    );
  },
  download: async (id) => {
    unwrapTts(await commands.downloadTtsModel(id));
    await useSettingsStore.getState().refreshSettingsChecked();
  },
  cancel: async (id) => {
    unwrapTts(await commands.cancelTtsDownload(id));
  },
});
export function ModelsPage() {
  const { t } = useTranslation();
  const { refreshSettingsChecked } = useSettings();
  const [models, setModels] = useState<TtsModelInfo[]>([]);
  const [available, setAvailable] = useState<Device[]>(["cpu"]);
  const downloadState = useSyncExternalStore(
    downloads.subscribe,
    downloads.snapshot,
  );
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState("");
  const refresh = async () => {
    setModels(unwrapTts(await commands.getTtsModels()));
  };
  useEffect(() => {
    let alive = true;
    const update = async () => {
      try {
        const next = unwrapTts(await commands.getTtsModels());
        if (alive) setModels(next);
        await downloads.refresh();
      } catch {
        if (alive) setError("tts.errors.generic");
      }
    };
    void update();
    void commands
      .getTtsDevices()
      .then(unwrapTts)
      .then((devices) => {
        if (alive) setAvailable(devices);
      })
      .catch(() => {});
    const timer = setInterval(() => void update(), 500);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);
  const run = async (job: Promise<Result<unknown, string>>) => {
    try {
      unwrapTts(await job);
      await refresh();
      await refreshSettingsChecked();
      setError(null);
    } catch (reason) {
      setError(
        reason instanceof Error && reason.message.startsWith("tts.errors.")
          ? reason.message
          : "tts.errors.generic",
      );
    }
  };
  return (
    <section className="w-full max-w-3xl flex flex-col gap-4">
      <h2>{t("tts.modelsTitle")}</h2>
      <label>
        {t("tts.token")}
        <input
          type="password"
          autoComplete="off"
          value={token}
          onChange={(event) => setToken(event.target.value)}
        />
      </label>
      <p>{t("tts.tokenHelp")}</p>
      <button
        onClick={() => {
          void run(commands.setTtsHfToken(token));
          setToken("");
        }}
      >
        {t("tts.saveToken")}
      </button>
      {(error || downloadState.error) && (
        <p role="alert">
          {t(error ?? downloadState.error ?? "tts.errors.generic")}
        </p>
      )}
      {models.map((model) => (
        <ModelCard
          key={model.id}
          model={model}
          downloadProgress={downloadState.progress[model.id]}
          available={available}
          state={{
            selectedId: models.find((row) => row.active)?.id ?? "",
            downloadId: downloadState.active.includes(model.id)
              ? model.id
              : (downloadState.active[0] ?? null),
            deletingId,
          }}
          actions={{
            use: (id) => void run(commands.setTtsModel(id)),
            download: (id) => void downloads.download(id),
            delete: (id) => {
              setDeletingId(id);
              void run(commands.deleteTtsModel(id)).finally(() =>
                setDeletingId(null),
              );
            },
          }}
          runOn={(id, value) => void run(commands.setTtsRunOn(id, value))}
          cancel={(id) => void downloads.cancel(id)}
        />
      ))}
    </section>
  );
}
