export function createTokenPresence() {
  let present = false;
  const listeners = new Set<() => void>();
  return {
    snapshot: () => present,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    set: (value: boolean) => {
      present = value;
      listeners.forEach((listener) => listener());
    },
  };
}
export const tokenPresence = createTokenPresence();
export interface TokenEntryState {
  expanded: boolean;
  visible: boolean;
  draft: string;
  pending: boolean;
  error: string | null;
}
export function tokenEntry(
  presence: ReturnType<typeof createTokenPresence>,
  persist: (value: string) => Promise<boolean>,
) {
  let state: TokenEntryState = {
    expanded: false,
    visible: false,
    draft: "",
    pending: false,
    error: null,
  };
  const listeners = new Set<() => void>();
  const update = (patch: Partial<TokenEntryState>) => {
    state = { ...state, ...patch };
    listeners.forEach((listener) => listener());
  };
  const hide = () => update({ draft: "", expanded: false, visible: false });
  const save = async (value: string) => {
    if (state.pending) return false;
    update({ pending: true, error: null });
    try {
      presence.set(await persist(value));
      hide();
      return true;
    } catch {
      update({ error: "tts.errors.generic" });
      return false;
    } finally {
      update({ pending: false });
    }
  };
  return {
    snapshot: () => state,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    toggle: () => {
      if (state.pending) return;
      if (state.expanded) hide();
      else update({ expanded: true, error: null });
    },
    edit: (draft: string) => update({ draft }),
    show: () => update({ visible: !state.visible }),
    save: () => save(state.draft),
    clear: () => save(""),
    hide,
  };
}
export function groupModels<
  T extends { id: string; downloaded: boolean; downloading: boolean },
>(models: readonly T[], active: readonly string[]) {
  const downloaded = (model: T) =>
    model.downloaded && !active.includes(model.id) && !model.downloading;
  return {
    downloaded: models.filter(downloaded),
    available: models.filter((model) => !downloaded(model)),
  };
}
