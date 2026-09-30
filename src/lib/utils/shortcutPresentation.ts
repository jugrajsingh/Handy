export function dictationShortcutIds(enabled: boolean): string[] {
  return enabled
    ? ["transcribe_with_post_process", "transcribe"]
    : ["transcribe"];
}

export function shortcutLabelId(id: string, enabled: boolean): string {
  return id === "transcribe" && enabled ? "transcribe_raw" : id;
}

export function canClearShortcut(id: string, enabled: boolean): boolean {
  return (
    enabled && (id === "transcribe" || id === "transcribe_with_post_process")
  );
}
