import type { SessionSnapshot } from "@/bindings";
export function highlightedText(text: string, start: number, end: number) {
  const from = Math.max(0, Math.min(text.length, start));
  const to = Math.max(from, Math.min(text.length, end));
  return {
    before: text.slice(0, from),
    current: text.slice(from, to),
    after: text.slice(to),
  };
}
export function progressPresentation(snapshot: SessionSnapshot) {
  return {
    percent: Math.max(0, Math.min(100, snapshot.progress * 100)),
    elapsed: Math.round(snapshot.elapsed_seconds),
    remaining: Math.round(snapshot.remaining_seconds),
    stateKey: `tts.states.${snapshot.state}`,
  };
}
