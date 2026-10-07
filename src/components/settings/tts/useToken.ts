import { useEffect, useMemo, useSyncExternalStore } from "react";
import { commands } from "@/bindings";
import { tokenEntry, tokenPresence } from "./token";
export function useToken() {
  const entry = useMemo(
    () =>
      tokenEntry(tokenPresence, async (token) => {
        const result = await commands.setTtsHfToken(token);
        if (result.status === "error") throw new Error(result.error);
        return result.data;
      }),
    [],
  );
  const state = useSyncExternalStore(entry.subscribe, entry.snapshot);
  const present = useSyncExternalStore(
    tokenPresence.subscribe,
    tokenPresence.snapshot,
  );
  useEffect(() => () => entry.hide(), [entry]);
  return { entry, state, present };
}
