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
import { groupModels } from "./token";
import { tokenCardProps } from "./TokenField";
import { useToken } from "./useToken";
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
  const token = useToken();
  const groups = groupModels(models, downloadState.active);
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
      return true;
    } catch (reason) {
      setError(
        reason instanceof Error && reason.message.startsWith("tts.errors.")
          ? reason.message
          : "tts.errors.generic",
      );
      return false;
    }
  };
  return (
    <section className="w-full max-w-3xl flex flex-col gap-4">
      <div>
        <h1 className="text-xl font-semibold mb-2">{t("tts.modelsTitle")}</h1>
        <p className="text-text/60 text-sm">{t("tts.modelsSubtitle")}</p>
      </div>
      {(error || downloadState.error) && (
        <p role="alert">
          {t(error ?? downloadState.error ?? "tts.errors.generic")}
        </p>
      )}
      {([true, false] as const).map((downloaded) => (
        <div key={String(downloaded)} className="space-y-3">
          <h2 className="text-sm font-medium text-text/60">
            {t(downloaded ? "tts.downloaded" : "tts.availableToDownload")}
          </h2>
          {(downloaded ? groups.downloaded : groups.available).map((model) => (
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
              {...tokenCardProps(token)}
              downloadError={downloadState.errors[model.id]}
            />
          ))}
        </div>
      ))}
    </section>
  );
}
