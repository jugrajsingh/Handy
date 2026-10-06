import React, { useEffect, useState, useRef, useMemo } from "react";
import ReactDOM from "react-dom/client";
import { listen, emit } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useTranslation } from "react-i18next";
import { commands, type SessionSnapshot, type Result } from "@/bindings";
import { createPopupAdapter, type PopupEvent } from "./adapter";
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
          replaceText: async (value) => {
            unwrap(await commands.readAloudReplaceText(value));
          },
          close: async () => {
            unwrap(await commands.readAloudClose());
          },
          error: (key) => setSnapshot((old) => ({ ...old, error: key })),
        },
      ),
    [],
  );
  useEffect(() => {
    void getCurrentWindow().setTitle(t("tts.title"));
  }, [t]);
  useEffect(() => {
    let alive = true;
    const unlisten = listen<PopupEvent>("read-aloud-state", ({ payload }) => {
      if (alive) actions.event(payload);
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
  return (
    <ReadAloudPopup
      text={text}
      snapshot={snapshot}
      edit={(value) => {
        void actions.edit(value);
      }}
      toggle={() => {
        void actions.toggle();
      }}
      stop={() => {
        void actions.stop();
      }}
      close={() => {
        void actions.close();
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
