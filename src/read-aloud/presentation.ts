import type { SessionSnapshot } from "@/bindings";
export function highlightedText(text: string, start: number, end: number) {
  const from = Math.max(0, Math.min(text.length, start));
  const to = Math.max(from, Math.min(text.length, end));
  return {
    before: text.slice(0, from),
    current: text.slice(from, to),
    after: text.slice(to),
  };
}
export function progressPresentation(snapshot: SessionSnapshot) {
  return {
    percent: Math.max(0, Math.min(100, snapshot.progress * 100)),
    elapsed: Math.round(snapshot.elapsed_seconds),
    remaining: Math.round(snapshot.remaining_seconds),
    stateKey: `tts.states.${snapshot.state}`,
  };
}

export function popupMode(
  state: SessionSnapshot["state"],
  editing: boolean,
  error: string | null = null,
) {
  return !editing &&
    !error &&
    ["starting", "buffering", "playing", "paused"].includes(state)
    ? "reading"
    : "editing";
}

export function formatTime(seconds: number) {
  const whole = Math.max(0, Math.round(seconds));
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
}

export function pasteText(
  text: string,
  pasted: string,
  start = 0,
  end = text.length,
) {
  return text.slice(0, start) + pasted + text.slice(end);
}

export function enterPopupEdit(
  text: string,
  actions: {
    edit: (text: string) => void;
    setEditing: (value: boolean) => void;
  },
) {
  actions.setEditing(true);
  actions.edit(text);
}

export function togglePopupPlayback(actions: {
  toggle: () => void;
  setEditing: (value: boolean) => void;
}) {
  actions.setEditing(false);
  actions.toggle();
}
