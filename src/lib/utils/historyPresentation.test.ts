import assert from "node:assert/strict";
import {
  cleanedText,
  copyHistoryRaw,
  historyCopyText,
  historyModelName,
} from "./historyPresentation";
for (const post_processed_text of [null, "", " \n"]) {
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
assert.equal(failures, 1);
await copyHistoryRaw(async () => {
  throw new Error("clipboard failed");
}, onError);
assert.equal(failures, 2);
const equalEntry = {
  transcription_text: "raw",
  post_processed_text: "raw",
  post_process_provider: "local_llm",
  post_process_model: "s1-mini-q4km",
};
assert.equal(cleanedText(equalEntry), "raw");
assert.equal(historyCopyText(equalEntry), "raw");
const registry = [
  { id: "s1-mini-q4km", display_name: "S1-mini by Superwhisper" },
];
assert.equal(
  historyModelName(
    { post_process_provider: "local_llm", post_process_model: "s1-mini-q4km" },
    registry,
  ),
  "S1-mini by Superwhisper",
);
assert.equal(
  historyModelName(
    { post_process_provider: "openai", post_process_model: "s1-mini-q4km" },
    registry,
  ),
  "s1-mini-q4km",
);
assert.equal(
  historyModelName(
    { post_process_provider: "local_llm", post_process_model: "unknown" },
    registry,
  ),
  "unknown",
);
assert.equal(
  historyModelName(
    { post_process_provider: null, post_process_model: null },
    registry,
  ),
  null,
);
assert.equal(
  historyModelName(
    { post_process_provider: null, post_process_model: "s1-mini-q4km" },
    registry,
  ),
  "S1-mini by Superwhisper",
);
assert.equal(
  historyModelName(
    { post_process_provider: "local_llm", post_process_model: "s1-mini-q4km" },
    [],
  ),
  "s1-mini-q4km",
);
console.log("historyPresentation: all assertions passed");
