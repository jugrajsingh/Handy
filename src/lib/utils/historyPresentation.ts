import type { HistoryEntry, LocalLlmModelInfo } from "@/bindings";

export function cleanedText(
  entry: Pick<HistoryEntry, "transcription_text" | "post_processed_text">,
): string | null {
  const text = entry.post_processed_text;
  return text !== null && text.trim() !== "" ? text : null;
}

export function historyCopyText(
  entry: Pick<HistoryEntry, "transcription_text" | "post_processed_text">,
): string {
  return cleanedText(entry) ?? entry.transcription_text;
}

export async function copyHistoryRaw(
  copy: () => Promise<boolean>,
  onError: () => void,
): Promise<void> {
  try {
    if (!(await copy())) onError();
  } catch {
    onError();
  }
}

export function historyModelName(
  entry: Pick<HistoryEntry, "post_process_model" | "post_process_provider">,
  models: Pick<LocalLlmModelInfo, "id" | "display_name">[],
): string | null {
  const id = entry.post_process_model;
  if (id === null) return null;
  if (
    entry.post_process_provider !== null &&
    entry.post_process_provider !== "local_llm"
  )
    return id;
  return models.find((model) => model.id === id)?.display_name ?? id;
}
