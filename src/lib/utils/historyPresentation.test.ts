import assert from "node:assert/strict";
import {
  cleanedText,
  copyHistoryRaw,
  historyCopyText,
} from "./historyPresentation";
for (const post_processed_text of [null, "", " \n", "raw"]) {
  const entry = { transcription_text: "raw", post_processed_text };
  assert.equal(cleanedText(entry), null);
  assert.equal(historyCopyText(entry), "raw");
}
const entry = { transcription_text: "raw", post_processed_text: "Clean." };
assert.equal(cleanedText(entry), "Clean.");
assert.equal(historyCopyText(entry), "Clean.");
assert.equal(historyCopyText({ ...entry, transcription_text: "" }), "Clean.");
assert.equal(
  cleanedText({ ...entry, post_processed_text: " Clean. \n" }),
  " Clean. \n",
);
let failures = 0;
const onError = () => {
  failures++;
};
await copyHistoryRaw(async () => true, onError);
assert.equal(failures, 0);
await copyHistoryRaw(async () => false, onError);
await copyHistoryRaw(async () => {
  throw new Error("clipboard failed");
}, onError);
assert.equal(failures, 2);
console.log("historyPresentation: all assertions passed");
