import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

// Sidebar.tsx pulls in Vite-only i18n loading, so the section table is read as source.
const source = readFileSync(new URL("./Sidebar.tsx", import.meta.url), "utf8");
const table = source.slice(
  source.indexOf("export const SECTIONS_CONFIG"),
  source.indexOf("as const satisfies"),
);
const sections = [...table.matchAll(/^ {2}(\w+): \{[^}]*?icon: (\w+),/gms)].map(
  ([, id, icon]) => ({ id, icon }),
);
assert.ok(sections.length >= 9, `parsed ${sections.length} sidebar sections`);
for (const { id, icon } of sections) {
  const clash = sections.find(
    (other) => other.id !== id && other.icon === icon,
  );
  assert.equal(
    clash,
    undefined,
    `sections ${id} and ${clash?.id} share ${icon}`,
  );
}
console.log(`sidebar icons unique across ${sections.length} sections`);
