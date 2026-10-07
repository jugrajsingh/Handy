import assert from "node:assert/strict";
import { createTokenPresence, tokenEntry, groupModels } from "./token";
import { rowActions } from "./actions";
const presence = createTokenPresence();
let received = "";
let saveResult = true;
let fail = false;
const entry = tokenEntry(presence, async (value) => {
  received = value;
  if (fail) throw new Error("fixture");
  return saveResult;
});
assert.equal(entry.snapshot().expanded, false);
entry.toggle();
assert.equal(entry.snapshot().expanded, true, "Set token opens the entry");
entry.edit("fixture-token");
entry.show();
assert.equal(entry.snapshot().visible, true);
await entry.save();
assert.equal(received, "fixture-token");
assert.equal(
  presence.snapshot(),
  true,
  "successful backend result sets session presence",
);
assert.equal(entry.snapshot().draft, "", "save clears the password input");
assert.equal(entry.snapshot().expanded, false, "save hides the entry");
assert.equal(entry.snapshot().visible, false);
const reopened = tokenEntry(presence, async () => true);
assert.equal(reopened.snapshot().draft, "", "navigation retains no password");
assert.equal(presence.snapshot(), true);
entry.toggle();
entry.edit("rejected-fixture");
fail = true;
assert.equal(await entry.save(), false);
assert.equal(entry.snapshot().error, "tts.errors.generic");
assert.equal(presence.snapshot(), true, "failed save preserves presence");
assert.equal(entry.snapshot().expanded, true);
fail = false;
saveResult = false;
await entry.clear();
assert.equal(received, "", "Clear sends blank token to backend");
assert.equal(
  presence.snapshot(),
  false,
  "backend false clears session presence",
);
assert.equal(entry.snapshot().draft, "");
entry.toggle();
entry.edit("unsaved-fixture");
entry.toggle();
assert.equal(entry.snapshot().draft, "", "hiding erases an unsaved token");
assert.equal(
  createTokenPresence().snapshot(),
  false,
  "restart has no session presence",
);
const rows = [
  { id: "ready", downloaded: true, downloading: false },
  { id: "missing", downloaded: false, downloading: false },
  { id: "downloading", downloaded: true, downloading: true },
  { id: "active", downloaded: true, downloading: false },
];
const groups = groupModels(rows, ["active"]);
assert.deepEqual(
  groups.downloaded.map((row) => row.id),
  ["ready"],
);
assert.deepEqual(
  groups.available.map((row) => row.id),
  ["missing", "downloading", "active"],
);
console.log(
  "token entry, memory presence and model grouping assertions passed",
);

let downloadCalls = 0;
const ready = createTokenPresence();
const saver = tokenEntry(ready, async (value) => value.trim().length > 0);
const row = () =>
  rowActions(
    { id: "pocket-en", downloaded: false },
    { selectedId: "other", downloadId: null, deletingId: null },
    { use: () => {}, delete: () => {}, download: () => downloadCalls++ },
    ready.snapshot(),
  );
row().download();
assert.equal(downloadCalls, 0);
saver.edit("fixture");
await saver.save();
row().download();
assert.equal(downloadCalls, 1, "async token save enables Pocket Download");
await saver.clear();
row().download();
assert.equal(downloadCalls, 1, "Clear disables Pocket Download");
saver.edit("   ");
await saver.save();
assert.equal(ready.snapshot(), false, "empty save respects backend absence");
let release!: (value: boolean) => void;
const pending = tokenEntry(
  ready,
  () =>
    new Promise((resolve) => {
      release = resolve;
    }),
);
pending.toggle();
pending.edit("fixture");
const save = pending.save();
assert.equal(pending.snapshot().pending, true);
assert.equal(
  ready.snapshot(),
  false,
  "presence changes only after save success",
);
release(true);
await save;
assert.equal(ready.snapshot(), true);
