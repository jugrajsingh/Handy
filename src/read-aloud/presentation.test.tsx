import assert from "node:assert/strict";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import i18next from "i18next";
import { I18nextProvider } from "react-i18next";
import en from "../i18n/locales/en/translation.json";
import type { SessionSnapshot } from "../bindings";
import {
  highlightedText,
  progressPresentation,
  popupMode,
  formatTime,
  pasteText,
  enterPopupEdit,
  togglePopupPlayback,
} from "./presentation";
import { ReadAloudPopup } from "./ReadAloudPopup";
await i18next.init({ lng: "en", resources: { en: { translation: en } } });
const snapshot: SessionSnapshot = {
  epoch: 0,
  state: "buffering",
  source_start: 2,
  source_end: 7,
  elapsed_seconds: 9.4,
  remaining_seconds: 40.6,
  progress: 0.2,
  error: null,
  output_fallback: false,
  queued_audio_seconds: 1,
  buffering_pauses: 1,
  head_start_seconds: 4,
};
assert.deepEqual(highlightedText("A😀BC", 1, 3), {
  before: "A",
  current: "😀",
  after: "BC",
});
assert.deepEqual(highlightedText("abc", -1, 99), {
  before: "",
  current: "abc",
  after: "",
});
assert.equal(progressPresentation({ ...snapshot, progress: 2 }).percent, 100);
const render = (state: SessionSnapshot, editing = false) =>
  renderToStaticMarkup(
    <I18nextProvider i18n={i18next}>
      <ReadAloudPopup
        text="A sentence"
        modelName="Supertonic 3"
        editing={editing}
        enterEdit={() => {}}
        snapshot={state}
        edit={() => {}}
        toggle={() => {}}
        stop={() => {}}
        close={() => {}}
        models={() => {}}
      />
    </I18nextProvider>,
  );
const buffering = render(snapshot);
const status = buffering.match(/<span role="status"[^>]*>(.*?)<\/span>/s)?.[1];
assert.ok(status?.includes("Buffering"), status);
const mark = buffering.match(/<mark[^>]*>(.*?)<\/mark>/s)?.[1];
assert.equal(mark, "sente");
const alert = render({ ...snapshot, error: "tts.errors.notDownloaded" }).match(
  /<div role="alert"[^>]*>(.*?)<\/div>/s,
)?.[1];
assert.ok(alert?.includes(en.tts.openModels), alert);
const fallback = render({ ...snapshot, output_fallback: true });
const fallbackNote = fallback.match(
  /<span[^>]*data-output-fallback[^>]*>(.*?)<\/span>/s,
)?.[1];
assert.equal(fallbackNote, en.tts.outputFallback);
console.log(
  "popup presentation: UTF-16/highlight/Buffering/error/fallback assertions passed",
);

for (const state of [
  "waiting",
  "starting",
  "buffering",
  "playing",
  "paused",
  "finished",
  "error",
] as const) {
  const mode = ["starting", "buffering", "playing", "paused"].includes(state)
    ? "reading"
    : "editing";
  assert.equal(popupMode(state, false), mode, state);
  assert.equal(popupMode(state, true), "editing", `${state} explicit edit`);
  for (const editing of [false, true]) {
    const html = render({ ...snapshot, state }, editing);
    const readonly = html.includes("data-source-preview");
    const textarea = html.includes("<textarea");
    assert.notEqual(readonly, textarea, `${state}: exactly one text view`);
    assert.equal(
      textarea,
      editing || mode === "editing",
      `${state}: expected text view`,
    );
  }
}
assert.equal(popupMode("playing", false, "tts.errors.generic"), "editing");
for (const [seconds, expected] of [
  [0, "0:00"],
  [59, "0:59"],
  [60, "1:00"],
  [3599, "59:59"],
] as const)
  assert.equal(formatTime(seconds), expected);
assert.ok(
  buffering.match(/<span class="tabular-nums[^>]*>(.*?)<\/span>/s)?.[1] ===
    "0:09 / 0:50",
  "elapsed plus remaining gives total",
);
const progressBar = buffering.match(/<div[^>]*role="progressbar"[^>]*>/)?.[0];
assert.ok(
  progressBar?.includes('aria-label="Progress"') &&
    progressBar.includes('aria-valuemax="100"') &&
    progressBar.includes('aria-valuenow="20"'),
  progressBar,
);
const header = buffering.match(/<header[^>]*>(.*?)<\/header>/s)?.[1];
assert.ok(
  header?.includes("Supertonic 3") && header.includes('aria-label="Close"'),
);
assert.ok(
  buffering.match(/<mark[^>]*class="[^"]*bg-logo-primary/),
  "themed highlight",
);
assert.equal(pasteText("abcdef", "XY", 2, 4), "abXYef");
assert.equal(pasteText("abcdef", "new"), "new");
const edits: string[] = [];
let editingMode = false;
enterPopupEdit("keep text", {
  edit: (value) => edits.push(value),
  setEditing: (value) => {
    editingMode = value;
  },
});
assert.deepEqual(
  edits,
  ["keep text"],
  "click edit goes through revision-aware edit path",
);
assert.equal(editingMode, true);
let toggles = 0;
togglePopupPlayback({
  toggle: () => {
    toggles++;
  },
  setEditing: (value) => {
    editingMode = value;
  },
});
assert.equal(toggles, 1);
assert.equal(editingMode, false, "Play leaves edit mode");
console.log("popup mode, time, controls and edit/paste seam assertions passed");

const { registerPopupPaste, restorePopupEditor, routePopupPaste } =
  await import("./presentation");
const { createPopupAdapter } = await import("./adapter");
const { routePopupKey } = await import("./actions");
const listeners = new Set<
  (event: import("./presentation").PopupPasteEvent) => void
>();
const pasteWindow = {
  addEventListener: (
    _type: "paste",
    listener: (event: import("./presentation").PopupPasteEvent) => void,
  ) => {
    listeners.add(listener);
  },
  removeEventListener: (
    _type: "paste",
    listener: (event: import("./presentation").PopupPasteEvent) => void,
  ) => {
    listeners.delete(listener);
  },
};
const editorTarget = {};
const bodyTarget = {};
let view = {
  ...snapshot,
  state: "waiting" as SessionSnapshot["state"],
  text: "old text",
};
let explicitEdit = true;
let editorMounted = true;
let selection: { start: number; end: number } | null = null;
let pendingCaret: number | null = null;
let editingWrite = Promise.resolve();
const pasteTrace: string[] = [];
const replacements: { text: string; revision: number }[] = [];
const adapter = createPopupAdapter(
  () => view,
  (next) => {
    view = next;
  },
  {
    play: async () => {
      pasteTrace.push("play");
    },
    pause: async () => {},
    stop: async () => {
      pasteTrace.push("stop");
    },
    replaceText: async (text, revision) => {
      replacements.push({ text, revision });
    },
    close: async () => {},
    error: () => assert.fail("unexpected paste error"),
  },
);
const detachPaste = registerPopupPaste(pasteWindow, {
  isEditorTarget: (target) => editorMounted && target === editorTarget,
  getText: () => view.text,
  getSelection: () => selection,
  enterEdit: (text, caret) => {
    pendingCaret = caret;
    enterPopupEdit(text, {
      setEditing: (value) => {
        explicitEdit = value;
      },
      edit: (value) => {
        editingWrite = adapter.edit(value);
      },
    });
  },
});
routePopupKey(
  {
    key: "Enter",
    ctrlKey: true,
    defaultPrevented: false,
    preventDefault: () => {},
  },
  {
    close: () => {},
    toggle: () =>
      togglePopupPlayback({
        toggle: () => {
          editingWrite = adapter.toggle();
        },
        setEditing: (value) => {
          explicitEdit = value;
        },
      }),
  },
);
await editingWrite;
view = { ...view, state: "playing" };
editorMounted = false;
assert.equal(popupMode(view.state, explicitEdit), "reading");
const dispatchPaste = (target: unknown, text: string) => {
  let prevented = false;
  const event = {
    target,
    defaultPrevented: false,
    clipboardData: { getData: () => text },
    preventDefault: () => {
      prevented = true;
    },
  };
  for (const listener of listeners) listener(event);
  return prevented;
};
assert.equal(
  dispatchPaste(bodyTarget, "new text"),
  true,
  "window registration captures body paste after keyboard Play",
);
await editingWrite;
assert.deepEqual(pasteTrace, ["play", "stop"]);
assert.deepEqual(replacements, [{ text: "new text", revision: 1 }]);
assert.equal(explicitEdit, true);
assert.equal(view.text, "new text");
assert.equal(
  pendingCaret,
  8,
  "body paste prepares editor focus/caret at inserted text end",
);
editorMounted = true;
selection = { start: 4, end: 8 };
assert.equal(
  dispatchPaste(editorTarget, "words"),
  false,
  "Textarea paste retains native insertion",
);
assert.equal(
  replacements.length,
  1,
  "window listener does not also insert Textarea paste",
);
// The native input change sends its resulting value through the existing edit path.
await adapter.edit(pasteText(view.text, "words", 4, 8));
assert.deepEqual(
  replacements,
  [
    { text: "new text", revision: 1 },
    { text: "new words", revision: 2 },
  ],
  "Textarea paste inserts exactly once with one client revision",
);
selection = { start: 4, end: 9 };
assert.equal(dispatchPaste(bodyTarget, "text"), true);
await editingWrite;
assert.equal(
  view.text,
  "new text",
  "non-Textarea paste uses the retained editor selection",
);
assert.equal(pendingCaret, 8);
detachPaste();
assert.equal(
  listeners.size,
  0,
  "cleanup removes the registered window listener",
);
assert.equal(dispatchPaste(bodyTarget, "ignored"), false);
assert.equal(view.text, "new text", "unmounted listener cannot edit");
console.log(
  "window paste: keyboard Play/body target, revision, native exactly-once, caret and cleanup assertions passed",
);

const restored: (string | number[])[] = [];
restorePopupEditor(
  {
    focus: () => {
      restored.push("focus");
    },
    setSelectionRange: (start, end) => {
      restored.push([start, end]);
    },
  },
  { start: 8, end: 8 },
);
assert.deepEqual(
  restored,
  ["focus", [8, 8]],
  "pending paste caret restores editor focus and selection",
);
for (const event of [
  { defaultPrevented: true, clipboardData: { getData: () => "ignored" } },
  { defaultPrevented: false, clipboardData: null },
]) {
  routePopupPaste(
    {
      ...event,
      target: bodyTarget,
      preventDefault: () =>
        assert.fail("must not intercept unavailable/handled paste"),
    },
    {
      isEditorTarget: () => false,
      getText: () => "",
      getSelection: () => null,
      enterEdit: () => assert.fail("must not edit unavailable/handled paste"),
    },
  );
}
