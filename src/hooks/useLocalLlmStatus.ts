import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { commands, type LocalLlmStatus } from "@/bindings";
import { subscribeLocalLlmStatus } from "./localLlmStatusSubscription";

export function useLocalLlmStatus(): {
  status: LocalLlmStatus | null;
  error: string | null;
} {
  const [status, setStatus] = useState<LocalLlmStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const subscription = subscribeLocalLlmStatus({
      listen: (onStatus) =>
        listen<LocalLlmStatus>("local-llm-state-changed", ({ payload }) =>
          onStatus(payload),
        ),
      getStatus: commands.getLocalLlmStatus,
      setStatus,
      setError,
      onCleanupError: (reason) => {
        console.error("Failed to remove local LLM status listener:", reason);
      },
    });
    return subscription.stop;
  }, []);
  return { status, error };
}
