import assert from "node:assert/strict";
import {
  createPopupAdapter,
  type PopupView,
  type PopupAdapterDependencies,
} from "./adapter";

function deferred() {
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
async function flush() {
  for (let i = 0; i < 8; i++) await Promise.resolve();
}
const initial: PopupView = {
  text: "original",
  epoch: 1,
  state: "waiting",
  source_start: 0,
  source_end: 0,
  elapsed_seconds: 0,
  remaining_seconds: 0,
  progress: 0,
  error: null,
  output_fallback: false,
  queued_audio_seconds: 0,
  buffering_pauses: 0,
  head_start_seconds: null,
};
function fixture(
  dependencies: Partial<PopupAdapterDependencies> = {},
  state = initial,
) {
  let view = { ...state };
  const calls: string[] = [];
  const adapter = createPopupAdapter(
    () => view,
    (next) => {
      view = next;
    },
    {
      play: async () => {
        calls.push("play");
      },
      pause: async () => {
        calls.push("pause");
      },
      stop: async () => {
        calls.push("stop");
      },
      replaceText: async (text) => {
        calls.push(`replace:${text}`);
      },
      close: async () => {
        calls.push("close");
      },
      error: (key) => {
        calls.push(key);
      },
      ...dependencies,
    },
  );
  return { adapter, calls, view: () => view };
}
async function draftAcknowledgements() {
  const f = fixture();
  f.adapter.event({
    text: "original",
    visible: true,
    snapshot: initial,
    client_revision: null,
    document_id: 1,
  });
  await f.adapter.edit("A");
  const ab = f.adapter.edit("AB");
  f.adapter.event({
    client_revision: 1,
    document_id: 1,
    text: "A",
    visible: true,
    snapshot: { ...initial, epoch: 5 },
  });
  assert.equal(
    f.view().text,
    "AB",
    "dirty draft survives earlier replacement echo",
  );
  const abc = f.adapter.edit(`${f.view().text}C`);
  await Promise.all([ab, abc]);
  assert.equal(
    f.view().text,
    "ABC",
    "displayed draft retains every typed character",
  );
  assert.equal(
    f.calls[f.calls.length - 1],
    "replace:ABC",
    "latest replacement retains every typed character",
  );
  f.adapter.event({
    client_revision: 3,
    document_id: 1,
    text: "ABC",
    visible: true,
    snapshot: { ...initial, epoch: 8 },
  });
  assert.equal(f.view().text, "ABC");
  f.adapter.event({
    client_revision: 1,
    document_id: 1,
    text: "A",
    visible: true,
    snapshot: { ...initial, epoch: 5 },
  });
  assert.equal(f.view().text, "ABC", "acknowledgement rejects older epochs");
  f.adapter.event({
    client_revision: null,
    document_id: 2,
    text: "new document",
    visible: true,
    snapshot: { ...initial, epoch: 9 },
  });
  assert.equal(
    f.view().text,
    "new document",
    "genuinely new document replaces acknowledged text",
  );
  await f.adapter.close();
  f.adapter.event({
    client_revision: null,
    document_id: 2,
    text: "new document",
    visible: true,
    snapshot: { ...initial, epoch: 10 },
  });
  assert.equal(
    f.view().epoch,
    9,
    "late document echo cannot reopen a closed popup",
  );
  f.adapter.event({
    client_revision: null,
    document_id: 2,
    text: null,
    visible: false,
    snapshot: { ...initial, epoch: 11 },
  });
  f.adapter.event({
    client_revision: null,
    document_id: 3,
    text: "new document",
    visible: true,
    snapshot: { ...initial, epoch: 12 },
  });
  assert.equal(
    f.view().epoch,
    12,
    "new visibility cycle can reopen the same document",
  );
  console.log("popup adapter draft acknowledgement assertions passed");
}
async function editBeforePlay() {
  for (const state of ["waiting", "playing"] as const) {
    const stopped = deferred();
    const replaced = deferred();
    const trace: string[] = [];
    const f = fixture(
      {
        stop: async () => {
          trace.push("stop");
          await stopped.promise;
        },
        replaceText: async (text) => {
          trace.push(`replace:${text}`);
          await replaced.promise;
        },
        play: async () => {
          trace.push("play");
        },
        pause: async () => {
          trace.push("pause");
        },
      },
      { ...initial, state },
    );
    const edit = f.adapter.edit("new text");
    const toggle = f.adapter.toggle();
    assert.deepEqual(
      trace,
      ["stop"],
      `${state}: no playback before Stop/Replace completes`,
    );
    stopped.resolve();
    await flush();
    assert.deepEqual(
      trace,
      ["stop", "replace:new text"],
      `${state}: no playback before replacement acknowledgement`,
    );
    replaced.resolve();
    await Promise.all([edit, toggle]);
    assert.deepEqual(
      trace,
      ["stop", "replace:new text", "play"],
      `${state}: edit then toggle reads replaced text`,
    );
  }
  for (const cancel of ["close", "stop", "edit", "toggle"] as const) {
    const stopped = deferred();
    const f = fixture({
      stop: async () => {
        await stopped.promise;
      },
    });
    const edit = f.adapter.edit("new text");
    const toggle = f.adapter.toggle();
    const conflict =
      cancel === "edit" ? f.adapter.edit("newer text") : f.adapter[cancel]();
    stopped.resolve();
    await Promise.all([edit, toggle, conflict]);
    assert.equal(
      f.calls.includes("play"),
      false,
      `${cancel} cancels queued playback intent`,
    );
    assert.equal(
      f.calls.includes("pause"),
      false,
      `${cancel} never pauses the pre-edit document`,
    );
  }
  const rejected = fixture({
    replaceText: async () => {
      throw new Error("fixture");
    },
  });
  await Promise.all([
    rejected.adapter.edit("new text"),
    rejected.adapter.toggle(),
  ]);
  assert.equal(
    rejected.calls.includes("play"),
    false,
    "failed replacement cancels playback intent",
  );
  assert.ok(rejected.calls.includes("tts.errors.generic"));
  console.log(
    "popup adapter edit-before-play and cancellation assertions passed",
  );
}
async function repeatedTextAcknowledgements() {
  const replacements: { text: string; revision: number | undefined }[] = [];
  const f = fixture({
    replaceText: async (text, clientRevision?: number) => {
      replacements.push({ text, revision: clientRevision });
    },
  });
  const event = (
    text: string | null,
    epoch: number,
    client_revision: number | null,
  ) =>
    f.adapter.event({
      text,
      visible: true,
      snapshot: { ...initial, epoch },
      client_revision,
      document_id: 1,
    });
  event("original", 1, null);
  await f.adapter.edit("A");
  await f.adapter.edit("AB");
  await f.adapter.edit("A");
  event("A", 3, 1);
  assert.equal(
    f.view().text,
    "A",
    "first A echo cannot acknowledge the later A draft",
  );
  event("AB", 5, 2);
  assert.equal(
    f.view().text,
    "A",
    "repeated-text echoes preserve the backspaced A draft",
  );
  await f.adapter.edit(f.view().text + "C");
  assert.equal(f.view().text, "AC", "typing after backspace retains AC");
  assert.deepEqual(
    replacements[replacements.length - 1],
    { text: "AC", revision: 4 },
    "replacement carries AC and its monotonically increasing client revision",
  );
  assert.deepEqual(
    replacements.map((entry) => entry.revision),
    [1, 2, 3, 4],
  );
  event(null, 6, 2);
  assert.equal(
    f.view().text,
    "AC",
    "snapshot-only older revision preserves the draft",
  );
  event("original", 6, null);
  assert.equal(
    f.view().text,
    "AC",
    "revision-less echo for the current document preserves the draft",
  );
  event("AC", 7, 4);
  assert.equal(
    f.view().text,
    "AC",
    "coalesced latest acknowledgement settles the draft",
  );
  f.adapter.event({
    text: "clipboard",
    visible: true,
    snapshot: { ...initial, epoch: 8 },
    client_revision: null,
    document_id: 2,
  });
  assert.equal(
    f.view().text,
    "clipboard",
    "new document identity resets acknowledgement state",
  );
  await f.adapter.edit("clipboard edit");
  assert.equal(
    replacements[replacements.length - 1].revision,
    5,
    "client revisions remain monotonic across new documents",
  );
  event("AC", 9, 4);
  assert.equal(
    f.view().text,
    "clipboard edit",
    "older document cannot overwrite a newer draft",
  );
  console.log("popup adapter repeated-text revision assertions passed");
}
if (!process.argv[2] || process.argv[2] === "repeated")
  await repeatedTextAcknowledgements();
if (!process.argv[2] || process.argv[2] === "draft")
  await draftAcknowledgements();
if (!process.argv[2] || process.argv[2] === "ordering") await editBeforePlay();
