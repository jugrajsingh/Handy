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
  let editSequence = 0;
  let closed = false;
  let writes = Promise.resolve();
  const attempt = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch {
      dependencies.error("tts.errors.generic");
    }
  };
  return {
    async toggle() {
      if (pending || closed || !controls(getSnapshot()).canPlay) return;
      pending = true;
      try {
        await attempt(
          controls(getSnapshot()).isPlaying
            ? dependencies.pause
            : dependencies.play,
        );
      } finally {
        pending = false;
      }
    },
    edit(text: string) {
      const sequence = ++editSequence;
      const stopped = dependencies.stop();
      writes = writes
        .then(async () => {
          await stopped;
          if (sequence === editSequence && !closed)
            await dependencies.replaceText(text);
        })
        .catch(() => dependencies.error("tts.errors.generic"));
      return writes;
    },
    async play() {
      if (pending || closed || !controls(getSnapshot()).canPlay) return;
      pending = true;
      try {
        await attempt(dependencies.play);
      } finally {
        pending = false;
      }
    },
    async stop() {
      if (closed || !controls(getSnapshot()).canStop) return;
      await attempt(dependencies.stop);
    },
    async close() {
      closed = true;
      editSequence++;
      await attempt(dependencies.close);
    },
    event(snapshot: PopupSnapshot) {
      if (!closed && snapshot.epoch >= getSnapshot().epoch) apply(snapshot);
    },
    reopen(snapshot: PopupSnapshot) {
      closed = false;
      apply(snapshot);
    },
  };
}
