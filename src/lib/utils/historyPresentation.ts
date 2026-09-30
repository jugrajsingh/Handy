import type { HistoryEntry } from "@/bindings";

export function cleanedText(
  entry: Pick<HistoryEntry, "transcription_text" | "post_processed_text">,
): string | null {
  const text = entry.post_processed_text;
  return text !== null &&
    text.trim() !== "" &&
    text !== entry.transcription_text
    ? text
    : null;
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
