import assert from "node:assert/strict";
import {
  canClearShortcut,
  clearButtonState,
  dictationShortcutIds,
  shortcutLabelId,
} from "./shortcutPresentation";

assert.deepEqual(dictationShortcutIds(false), ["transcribe"]);
assert.deepEqual(dictationShortcutIds(true), [
  "transcribe_with_post_process",
  "transcribe",
]);
assert.equal(shortcutLabelId("transcribe", false), "transcribe");
assert.equal(shortcutLabelId("transcribe", true), "transcribe_raw");
for (const id of ["transcribe", "transcribe_with_post_process"]) {
  assert.equal(canClearShortcut(id, true), true);
  assert.equal(canClearShortcut(id, false), false);
}
assert.equal(canClearShortcut("cancel", true), false);
for (const id of ["transcribe", "transcribe_with_post_process", "cancel"]) {
  for (const enabled of [false, true]) {
    for (const isRecording of [false, true]) {
      for (const isUpdating of [false, true]) {
        for (const disabled of [false, true]) {
          assert.deepEqual(
            clearButtonState({
              enabled,
              id,
              isRecording,
              isUpdating,
              disabled,
            }),
            {
              visible: enabled && id !== "cancel",
              disabled: disabled || isRecording || isUpdating,
            },
          );
        }
      }
    }
  }
}
const { default: en } = await import("../../i18n/locales/en/translation.json");
assert.equal(
  en.settings.general.shortcut.bindings.transcribe_raw.name,
  "Raw Shortcut",
);
assert.equal(
  en.settings.general.shortcut.bindings.transcribe_with_post_process.name,
  "Post-processing Shortcut",
);
console.log("shortcutPresentation: all assertions passed");
