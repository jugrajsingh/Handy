import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { commands, type LocalLlmStatus } from "@/bindings";

export function useLocalLlmStatus(): {
  status: LocalLlmStatus | null;
  error: string | null;
} {
  const [status, setStatus] = useState<LocalLlmStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    let revision = 0;
    const subscription = listen<LocalLlmStatus>(
      "local-llm-state-changed",
      ({ payload }) => {
        revision++;
        if (alive) setStatus(payload);
      },
    );
    void subscription
      .then(async () => {
        const before = revision;
        const initial = await commands.getLocalLlmStatus();
        if (alive && before === revision) setStatus(initial);
      })
      .catch((reason: unknown) => {
        if (alive)
          setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => {
      alive = false;
      void subscription
        .then((unlisten) => unlisten())
        .catch((reason: unknown) => {
          console.error("Failed to remove local LLM status listener:", reason);
        });
    };
  }, []);
  return { status, error };
}
