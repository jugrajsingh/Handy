export type DiffToken = { kind: "equal" | "removed" | "added"; text: string };
export type WordDiff =
  | { kind: "diff"; tokens: DiffToken[] }
  | { kind: "side_by_side" };
export const MAX_DIFF_TOKENS = 4000;

export function wordDiff(raw: string, cleaned: string): WordDiff {
  const a = raw.match(/\S+/gu) ?? [];
  const b = cleaned.match(/\S+/gu) ?? [];
  if (a.length > MAX_DIFF_TOKENS || b.length > MAX_DIFF_TOKENS) {
    return { kind: "side_by_side" };
  }
  const width = b.length + 1;
  const table = new Uint16Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      table[i * width + j] =
        a[i] === b[j]
          ? 1 + table[(i + 1) * width + j + 1]
          : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
  }
  const tokens: DiffToken[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      tokens.push({ kind: "equal", text: a[i++] });
      j++;
    } else if (
      i < a.length &&
      (j === b.length || table[(i + 1) * width + j] >= table[i * width + j + 1])
    ) {
      tokens.push({ kind: "removed", text: a[i++] });
    } else {
      tokens.push({ kind: "added", text: b[j++] });
    }
  }
  return { kind: "diff", tokens };
}
