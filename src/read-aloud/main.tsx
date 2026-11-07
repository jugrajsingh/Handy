import React, { useEffect, useState, useRef, useMemo } from "react";
import ReactDOM from "react-dom/client";
import { listen, emit } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useTranslation } from "react-i18next";
import { commands, type SessionSnapshot, type Result } from "@/bindings";
import { createPopupAdapter, type PopupEvent } from "./adapter";
import { routePopupKey } from "./actions";
import { enterPopupEdit, togglePopupPlayback } from "./presentation";
import { ReadAloudPopup } from "./ReadAloudPopup";
import "@/i18n";
import "@/App.css";
const initial: SessionSnapshot = {
  epoch: 0,
  state: "waiting",
  source_start: 0,
  source_end: 0,
  elapsed_seconds: 0,
  remaining_seconds: 0,
  progress: 0,
  error: null,
  output_fallback: false,
  queued_audio_seconds: 0,
  buffering_pauses: 0,
  head_start_seconds: null,
};
function unwrap<T>(result: Result<T, string>): T {
  if (result.status === "error") throw new Error(result.error);
  return result.data;
}
function PopupApp() {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [editing, setEditing] = useState(false);
  const [modelName, setModelName] = useState<string>();
  const documentId = useRef<number | null>(null);
  const [snapshot, setSnapshot] = useState(initial);
  const current = useRef({ ...initial, text: "" });
  current.current = { ...snapshot, text };
  const actions = useMemo(
    () =>
      createPopupAdapter(
        () => current.current,
        (next) => {
          current.current = { ...current.current, ...next };
          setText(next.text);
          setSnapshot((old) => ({ ...old, ...next }));
        },
        {
          play: async () => {
            unwrap(await commands.readAloudPlay());
          },
          pause: async () => {
            unwrap(await commands.readAloudPause());
          },
          stop: async () => {
            unwrap(await commands.readAloudStop());
          },
          replaceText: async (value, clientRevision) => {
            unwrap(await commands.readAloudReplaceText(value, clientRevision));
          },
          close: async () => {
            unwrap(await commands.readAloudClose());
          },
          readClipboard: async () => {
            unwrap(await commands.readAloudReadClipboard());
          },
          error: (key) => setSnapshot((old) => ({ ...old, error: key })),
        },
      ),
    [],
  );
  useEffect(() => {
    const keydown = (event: KeyboardEvent) =>
      routePopupKey(event, {
        close: actions.close,
        toggle: () =>
          togglePopupPlayback({
            toggle: () => void actions.toggle(),
            setEditing,
          }),
      });
    window.addEventListener("keydown", keydown);
    return () => window.removeEventListener("keydown", keydown);
  }, [actions]);
  useEffect(() => {
    void getCurrentWindow().setTitle(t("tts.title"));
  }, [t]);
  useEffect(() => {
    let alive = true;
    const unlisten = listen<PopupEvent>("read-aloud-state", ({ payload }) => {
      if (alive) {
        if (
          payload.snapshot.epoch >= current.current.epoch &&
          payload.document_id > (documentId.current ?? -1) &&
          payload.client_revision === null
        ) {
          documentId.current = payload.document_id;
          setEditing(false);
        }
        actions.event(payload);
      }
    });
    void commands
      .getReadAloudState()
      .then(unwrap)
      .then((view) => {
        if (alive) actions.event(view);
      })
      .catch(() => {});
    return () => {
      alive = false;
      void unlisten.then((stop) => stop());
    };
  }, [actions]);
  useEffect(() => {
    let alive = true;
    setModelName(undefined);
    void Promise.all([
      commands.getAppSettings().then(unwrap),
      commands.getTtsModels().then(unwrap),
    ])
      .then(([settings, models]) => {
        if (alive)
          setModelName(
            models.find(
              (model) => model.id === (settings.tts_model_id ?? "pocket-en"),
            )?.display_name,
          );
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [snapshot.epoch]);
  return (
    <ReadAloudPopup
      text={text}
      modelName={modelName}
      editing={editing}
      enterEdit={(value) =>
        enterPopupEdit(value, {
          edit: (next) => void actions.edit(next),
          setEditing,
        })
      }
      snapshot={snapshot}
      edit={(value) => {
        void actions.edit(value);
      }}
      toggle={() => {
        togglePopupPlayback({
          toggle: () => void actions.toggle(),
          setEditing,
        });
      }}
      stop={() => {
        void actions.stop();
      }}
      close={() => {
        void actions.close();
      }}
      readClipboard={() => {
        void actions.readClipboard();
      }}
      models={() => {
        void emit("open-tts-models");
        void actions.close();
      }}
    />
  );
}
const root = document.getElementById("root");
if (root)
  ReactDOM.createRoot(root).render(
    <React.StrictMode>
      <PopupApp />
    </React.StrictMode>,
  );
