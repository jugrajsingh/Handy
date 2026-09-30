import type { LocalLlmStatus } from "@/bindings";

export interface LocalLlmStatusDependencies {
  listen: (onStatus: (status: LocalLlmStatus) => void) => Promise<() => void>;
  getStatus: () => Promise<LocalLlmStatus>;
  setStatus: (status: LocalLlmStatus) => void;
  setError: (message: string) => void;
  onCleanupError: (error: unknown) => void;
}

export function subscribeLocalLlmStatus(deps: LocalLlmStatusDependencies) {
  let alive = true;
  let revision = 0;
  const subscription = deps.listen((status) => {
    revision++;
    if (alive) deps.setStatus(status);
  });
  const ready = subscription
    .then(async () => {
      const before = revision;
      const initial = await deps.getStatus();
      if (alive && before === revision) deps.setStatus(initial);
    })
    .catch((error: unknown) => {
      if (alive)
        deps.setError(error instanceof Error ? error.message : String(error));
    });
  const stop = () => {
    alive = false;
    void subscription.then((unlisten) => unlisten()).catch(deps.onCleanupError);
  };
  return { ready, stop };
}
