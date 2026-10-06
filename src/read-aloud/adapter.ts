import type { SessionSnapshot } from "@/bindings";
import { createPopupActions, type PopupDependencies } from "./actions";

export type PopupView = SessionSnapshot & { text: string };

export interface PopupEvent {
  text: string | null;
  visible: boolean;
  snapshot: SessionSnapshot;
}

export function createPopupAdapter(
  getSnapshot: () => PopupView,
  apply: (snapshot: PopupView) => void,
  dependencies: PopupDependencies,
) {
  let revision = 0;
  let acknowledged = 0;
  let acknowledgedText = getSnapshot().text;
  let visible = false;
  let epoch = getSnapshot().epoch;
  let replacements: { revision: number; text: string }[] = [];
  const actions = createPopupActions(
    getSnapshot,
    (next) => apply({ ...getSnapshot(), ...next }),
    {
      ...dependencies,
      async replaceText(text) {
        const sent = { revision, text };
        replacements.push(sent);
        try {
          await dependencies.replaceText(text);
        } catch (error) {
          const index = replacements.indexOf(sent);
          if (index !== -1) replacements.splice(index, 1);
          throw error;
        }
      },
    },
  );
  return {
    ...actions,
    edit(text: string) {
      revision++;
      apply({
        ...getSnapshot(),
        text,
        state: "waiting",
        source_start: 0,
        source_end: 0,
        elapsed_seconds: 0,
        remaining_seconds: 0,
        progress: 0,
        error: null,
        queued_audio_seconds: 0,
        head_start_seconds: null,
      });
      return actions.edit(text);
    },
    event(payload: PopupEvent) {
      if (payload.snapshot.epoch < epoch) return;
      epoch = payload.snapshot.epoch;
      const opened = payload.visible && !visible;
      visible = payload.visible;
      const sent = replacements
        .slice()
        .reverse()
        .find((entry) => entry.text === payload.text);
      const echo = sent !== undefined || payload.text === acknowledgedText;
      if (sent && sent.revision >= acknowledged) {
        acknowledged = sent.revision;
        acknowledgedText = sent.text;
        replacements = replacements.filter(
          (entry) => entry.revision > acknowledged,
        );
      }
      if (
        payload.visible &&
        payload.text !== null &&
        !sent &&
        ((opened && revision === acknowledged) || !echo)
      ) {
        revision = 0;
        acknowledged = 0;
        acknowledgedText = payload.text;
        replacements.length = 0;
        actions.reopen({ ...payload.snapshot, text: payload.text });
        return;
      }
      const dirty = revision > acknowledged;
      const next = dirty
        ? { ...getSnapshot(), epoch: payload.snapshot.epoch }
        : { ...payload.snapshot, text: payload.text ?? getSnapshot().text };
      actions.event(next);
    },
  };
}
