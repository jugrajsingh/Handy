export type ReadState =
  | "waiting"
  | "starting"
  | "buffering"
  | "playing"
  | "paused"
  | "finished"
  | "error";
export interface PopupSnapshot {
  epoch: number;
  state: ReadState;
  text: string;
  source_start: number;
  source_end: number;
}
export interface PopupDependencies {
  play: () => Promise<void>;
  pause: () => Promise<void>;
  stop: () => Promise<void>;
  replaceText: (text: string) => Promise<void>;
  close: () => Promise<void>;
  readClipboard: () => Promise<void>;
  error: (key: string) => void;
}
export function controls(snapshot: PopupSnapshot) {
  return {
    canPlay: snapshot.text.trim().length > 0 && snapshot.state !== "starting",
    canStop: ["starting", "buffering", "playing", "paused", "error"].includes(
      snapshot.state,
    ),
    isPlaying: snapshot.state === "playing" || snapshot.state === "buffering",
  };
}
export function createPopupActions(
  getSnapshot: () => PopupSnapshot,
  apply: (snapshot: PopupSnapshot) => void,
  dependencies: PopupDependencies,
) {
  let pending = false;
  let intent = 0;
  let editSequence = 0;
  let editing = false;
  let localStopped = false;
  let replacementReady = true;
  let closed = false;
  let writes = Promise.resolve();
  const snapshot = () => {
    const current = getSnapshot();
    return localStopped ? { ...current, state: "waiting" as const } : current;
  };
  const cancelIntent = () => {
    intent++;
    pending = false;
  };
  const attempt = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch {
      dependencies.error("tts.errors.generic");
    }
  };
  const playback = async (toggle: boolean) => {
    pending = true;
    const requested = ++intent;
    const pause = toggle && controls(snapshot()).isPlaying;
    try {
      if (editing) await writes;
      if (requested !== intent || closed || !replacementReady) return;
      localStopped = false;
      await attempt(pause ? dependencies.pause : dependencies.play);
    } finally {
      if (requested === intent) pending = false;
    }
  };
  return {
    async toggle() {
      if (pending && editing) {
        cancelIntent();
        return;
      }
      if (pending || closed || !controls(snapshot()).canPlay) return;
      await playback(true);
    },
    edit(text: string) {
      const sequence = ++editSequence;
      cancelIntent();
      editing = true;
      localStopped = true;
      replacementReady = false;
      const stopped = dependencies.stop().then(
        () => true,
        () => false,
      );
      writes = writes
        .then(async () => {
          if (!(await stopped)) throw new Error("Stop failed");
          if (sequence !== editSequence || closed) return;
          await dependencies.replaceText(text);
          if (sequence === editSequence && !closed) replacementReady = true;
        })
        .catch(() => {
          if (sequence === editSequence && !closed)
            dependencies.error("tts.errors.generic");
        })
        .finally(() => {
          if (sequence === editSequence) editing = false;
        });
      return writes;
    },
    async play() {
      if (pending || closed || !controls(snapshot()).canPlay) return;
      await playback(false);
    },
    async stop() {
      cancelIntent();
      if (closed || !controls(snapshot()).canStop) return;
      await attempt(dependencies.stop);
    },
    async readClipboard() {
      if (closed) return;
      cancelIntent();
      const requested = intent;
      if (editing) await writes;
      if (requested !== intent || closed) return;
      await attempt(dependencies.readClipboard);
    },
    async close() {
      closed = true;
      editSequence++;
      cancelIntent();
      await attempt(dependencies.close);
    },
    event(next: PopupSnapshot) {
      if (!closed && next.epoch >= getSnapshot().epoch) {
        if (!editing) localStopped = false;
        apply(next);
      }
    },
    reopen(next: PopupSnapshot) {
      closed = false;
      editSequence++;
      cancelIntent();
      editing = false;
      localStopped = false;
      replacementReady = true;
      apply(next);
    },
  };
}

export interface PopupKey {
  key: string;
  ctrlKey: boolean;
  defaultPrevented: boolean;
  preventDefault: () => void;
}
export function routePopupKey(
  event: PopupKey,
  actions: {
    close: () => void | Promise<void>;
    toggle: () => void | Promise<void>;
  },
) {
  if (event.defaultPrevented) return;
  if (event.key === "Escape") {
    event.preventDefault();
    void actions.close();
  } else if (event.key === "Enter" && event.ctrlKey) {
    event.preventDefault();
    void actions.toggle();
  }
}
