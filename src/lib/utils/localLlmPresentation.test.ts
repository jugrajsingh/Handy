import assert from "node:assert/strict";
import type {
  LocalLlmModelInfo,
  LocalLlmStatus,
  ModelInfo,
  Result,
} from "@/bindings";
import {
  cleanupStatus,
  cleanupPickerPresentation,
  createCleanupModelPickerActions,
  providerGroupKey,
  selectedDownloadedModel,
  type CleanupModelPickerDependencies,
} from "./localLlmPresentation";

const model: LocalLlmModelInfo = {
  id: "s1-mini-q4km",
  display_name: "S1-mini",
  downloaded: true,
  attribution: "Superwhisper",
  card_url: "https://superwhisper.com",
  size_bytes: 484219808,
  prompt_style: "s1_control_line",
};
assert.equal(
  selectedDownloadedModel([model], model.id)?.display_name,
  "S1-mini",
);
assert.equal(
  selectedDownloadedModel([{ ...model, downloaded: false }], model.id),
  null,
);
assert.equal(selectedDownloadedModel([model], "deleted-id"), null);
assert.equal(selectedDownloadedModel([], model.id), null);
assert.equal(selectedDownloadedModel([model], null), null);
for (const [state, expected] of [
  ["ready", "ready"],
  ["starting", "loading"],
  ["stopping", "loading"],
  ["failed", "error"],
  ["unloaded", "unloaded"],
] as const) {
  const status: LocalLlmStatus = { state, model_id: model.id, error: null };
  assert.equal(cleanupStatus(status), expected);
}
assert.equal(cleanupStatus(null), "unloaded");
assert.equal(
  providerGroupKey("local_llm"),
  "settings.postProcessing.localLlm.groupTitle",
);
assert.equal(providerGroupKey("openai"), "settings.postProcessing.api.title");
assert.equal(
  providerGroupKey("apple_intelligence"),
  "settings.postProcessing.api.title",
);

const models = [
  model,
  { ...model, id: "downloaded-b", display_name: "Alternative B" },
  { ...model, id: "downloaded-c", display_name: "Alternative C" },
  { ...model, id: "missing", downloaded: false },
];
const ready: LocalLlmStatus = {
  state: "ready",
  model_id: model.id,
  error: null,
};
for (const selectedId of ["deleted-id", "missing", null]) {
  const view = cleanupPickerPresentation(
    models,
    selectedId,
    true,
    "local_llm",
    ready,
  );
  assert.equal(view.visible, true);
  assert.equal(
    view.selected,
    null,
    "stale or deleted selection is never Active",
  );
  assert.equal(view.status, "none");
  assert.deepEqual(
    view.downloaded.map((entry) => entry.id),
    [model.id, "downloaded-b", "downloaded-c"],
  );
}
for (const enabled of [false, true]) {
  for (const provider of [
    "local_llm",
    "openai",
    "apple_intelligence",
    undefined,
  ]) {
    assert.equal(
      cleanupPickerPresentation(models, model.id, enabled, provider, ready)
        .visible,
      enabled && provider === "local_llm",
    );
  }
}
assert.equal(
  cleanupPickerPresentation(models, model.id, true, "local_llm", ready).status,
  "ready",
);

const ok: Result<null, string> = { status: "ok", data: null };
function setup(overrides: Partial<CleanupModelPickerDependencies> = {}) {
  const calls: string[] = [];
  const errors: (string | null)[] = [];
  const selecting: boolean[] = [];
  let open = false;
  const deps: CleanupModelPickerDependencies = {
    getModels: () => models,
    getOpen: () => open,
    setOpen: (value) => {
      open = value;
      calls.push(`open:${value}`);
    },
    setSelecting: (value) => selecting.push(value),
    setError: (value) => errors.push(value),
    onOpenPostProcessing: () => calls.push("navigate"),
    setLocalLlmModel: async (id) => {
      calls.push(`select:${id}`);
      return ok;
    },
    refreshSettingsChecked: async () => {
      calls.push("settings");
    },
    reloadChecked: async () => {
      calls.push("registry");
    },
    ...overrides,
  };
  return {
    actions: createCleanupModelPickerActions(deps),
    calls,
    errors,
    selecting,
    isOpen: () => open,
  };
}
for (const entry of models.filter((entry) => entry.downloaded)) {
  const state = setup();
  await state.actions.toggle();
  assert.deepEqual(state.calls, ["open:true", "registry"]);
  await state.actions.select(entry.id);
  assert.deepEqual(state.calls, [
    "open:true",
    "registry",
    `select:${entry.id}`,
    "settings",
    "registry",
    "open:false",
  ]);
  assert.deepEqual(state.selecting, [true, false]);
  assert.deepEqual(state.errors, [null, null]);
}
for (const entries of [[], [{ ...model, downloaded: false }]]) {
  const state = setup({ getModels: () => entries });
  await state.actions.toggle();
  assert.deepEqual(
    state.calls,
    ["navigate"],
    "no downloads opens post-processing settings",
  );
  assert.equal(state.isOpen(), false);
}
const closing = setup();
await closing.actions.toggle();
await closing.actions.toggle();
assert.deepEqual(closing.calls, ["open:true", "registry", "open:false"]);
for (const failure of ["result", "transport", "refresh", "reload"] as const) {
  const message = `${failure} failed`;
  const operations: string[] = [];
  const state = setup({
    setLocalLlmModel: async (id) => {
      operations.push(`select:${id}`);
      if (failure === "transport") throw new Error(message);
      if (failure === "result") return { status: "error", error: message };
      return ok;
    },
    refreshSettingsChecked: async () => {
      operations.push("settings");
      if (failure === "refresh") throw new Error(message);
    },
    reloadChecked: async () => {
      operations.push("registry");
      if (failure === "reload") throw message;
    },
  });
  await state.actions.toggle();
  await state.actions.select("downloaded-b");
  assert.equal(state.errors[state.errors.length - 1], message);
  assert.equal(state.isOpen(), true, "failed actions keep the picker open");
  assert.deepEqual(state.selecting, [true, false]);
  assert.deepEqual(
    operations,
    [
      "registry",
      "select:downloaded-b",
      ...(failure === "refresh" || failure === "reload" ? ["settings"] : []),
      ...(failure === "reload" ? ["registry"] : []),
    ],
    "failed dependencies stop the remaining selection sequence",
  );
}
const openFailure = setup({
  reloadChecked: async () => {
    throw new Error("open reload failed");
  },
});
await openFailure.actions.toggle();
assert.deepEqual(openFailure.errors, [null, "open reload failed"]);
assert.equal(openFailure.isOpen(), true);
for (const id of ["missing", "deleted-id"]) {
  const state = setup();
  await state.actions.select(id);
  assert.deepEqual(
    state.calls,
    [],
    "only downloaded registry entries can be selected",
  );
  assert.deepEqual(state.selecting, []);
}
let finishRefresh!: () => void;
const pendingRefresh = new Promise<void>((resolve) => {
  finishRefresh = resolve;
});
const busy = setup({ refreshSettingsChecked: () => pendingRefresh });
await busy.actions.toggle();
const pending = busy.actions.select(model.id);
await Promise.resolve();
assert.deepEqual(
  busy.selecting,
  [true],
  "selection stays busy through refresh",
);
await busy.actions.select("downloaded-b");
await busy.actions.toggle();
assert.deepEqual(
  busy.calls,
  ["open:true", "registry", `select:${model.id}`],
  "busy action cannot issue a second command",
);
finishRefresh();
await pending;
assert.deepEqual(busy.selecting, [true, false]);
assert.equal(busy.isOpen(), false);
await busy.actions.select("downloaded-c");
assert.equal(
  busy.calls.includes("select:downloaded-c"),
  true,
  "busy guard releases after completion",
);
console.log("localLlmPresentation: all assertions passed");

const { promptControls } = await import("./localLlmPresentation");
const prompts = [
  { id: "p", name: "Prompt", prompt: "Return prose. ${output}" },
];
const plain = { ...model, prompt_style: "plain_system_prompt" as const };
assert.deepEqual(promptControls("local_llm", undefined, prompts, null), {
  hidePrompts: true,
  showS1Controls: false,
  missingPrompt: false,
});
assert.deepEqual(promptControls("openai", undefined, prompts, null), {
  hidePrompts: false,
  showS1Controls: false,
  missingPrompt: false,
});
assert.deepEqual(promptControls("local_llm", model, prompts, "p"), {
  hidePrompts: true,
  showS1Controls: true,
  missingPrompt: false,
});
assert.deepEqual(promptControls("local_llm", plain, prompts, "p"), {
  hidePrompts: false,
  showS1Controls: false,
  missingPrompt: false,
});
for (const selected of [null, "missing"]) {
  assert.equal(
    promptControls("local_llm", plain, prompts, selected).missingPrompt,
    true,
  );
}
for (const text of ["", " \n\t ", " ${output} "]) {
  assert.equal(
    promptControls("local_llm", plain, [{ ...prompts[0], prompt: text }], "p")
      .missingPrompt,
    true,
  );
}
assert.equal(promptControls("openai", model, prompts, "p").hidePrompts, false);
assert.equal(
  promptControls("local_llm", null, prompts, null).hidePrompts,
  false,
);
console.log("promptControls: all assertions passed");

const React = await import("react");
const { renderToStaticMarkup } = await import("react-dom/server");
const { default: i18next } = await import("i18next");
const { initReactI18next } = await import("react-i18next");
const { default: en } = await import("../../i18n/locales/en/translation.json");
await i18next.use(initReactI18next).init({
  lng: "en",
  resources: { en: { translation: en } },
  interpolation: { escapeValue: false },
});
const { default: ModelStatusButton, modelStatusTitle } = await import(
  "../../components/model-selector/ModelStatusButton"
);
const { LocalLlmDropdown } = await import(
  "../../components/model-selector/LocalLlmDropdown"
);
const longName = "Qwen3-4B-Instruct-2507 with a very long model display name";
const button = renderToStaticMarkup(
  React.createElement(ModelStatusButton, {
    status: "ready",
    displayText: longName,
    isDropdownOpen: false,
    onClick: () => undefined,
    icon: React.createElement("svg", { "data-picker-icon": "post-processing" }),
  }),
);
const fullTitle = `Model status: ${longName}`;
assert.equal(
  modelStatusTitle({ scrollWidth: 300, clientWidth: 100 }, fullTitle),
  fullTitle,
);
assert.equal(
  modelStatusTitle({ scrollWidth: 100, clientWidth: 100 }, fullTitle),
  undefined,
);
assert.equal(
  modelStatusTitle({ scrollWidth: 80, clientWidth: 100 }, fullTitle),
  undefined,
);
assert.equal(modelStatusTitle(null, fullTitle), undefined);
assert.ok(!button.includes("title="));
assert.ok(button.includes("data-picker-icon"));
assert.ok(button.indexOf("data-picker-icon") < button.indexOf("rounded-full"));
assert.ok(button.includes("flex-1 min-w-0 truncate"));
assert.ok(!button.includes("max-w-28"));
const row = renderToStaticMarkup(
  React.createElement(LocalLlmDropdown, {
    models: [{ ...model, display_name: longName, size_bytes: 2497281120 }],
    selectedId: model.id,
    disabled: false,
    onSelect: () => undefined,
  }),
);
const onlyRow = row.match(/<button\b[\s\S]*?<\/button>/u)?.[0];
assert.ok(onlyRow);
assert.ok(onlyRow.includes(longName));
assert.ok(onlyRow.includes("Uses styling, structure and context controls."));
assert.ok(onlyRow.includes("2382 MiB"));
assert.ok(onlyRow.includes(">Active</span>"));
const missingRows = renderToStaticMarkup(
  React.createElement(LocalLlmDropdown, {
    models: [{ ...model, downloaded: false }],
    selectedId: model.id,
    disabled: false,
    onSelect: () => undefined,
  }),
);
assert.ok(!missingRows.includes("<button"));
console.log("footer rows and full-name tooltip: all assertions passed");

const plainRow = renderToStaticMarkup(
  React.createElement(LocalLlmDropdown, {
    models: [
      {
        ...model,
        display_name: "Quill 0.8B",
        prompt_style: "plain_system_prompt",
        size_bytes: 529296832,
      },
    ],
    selectedId: model.id,
    disabled: true,
    onSelect: () => undefined,
  }),
);
const plainButton = plainRow.match(/<button\b[\s\S]*?<\/button>/u)?.[0];
assert.ok(plainButton);
assert.ok(plainButton.includes("Uses your selected Handy prompt."));
assert.ok(plainButton.includes("Quill 0.8B"));
assert.ok(plainButton.includes("505 MiB"));
assert.ok(plainButton.includes(">Active</span>"));
assert.ok(plainButton.includes('disabled=""'));

assert.equal(
  cleanupPickerPresentation(models, "downloaded-b", true, "local_llm", ready)
    .status,
  "unloaded",
);
for (const state of ["starting", "stopping"] as const) {
  assert.equal(
    cleanupPickerPresentation(models, "downloaded-b", true, "local_llm", {
      ...ready,
      state,
    }).status,
    "unloaded",
  );
}

const { readFileSync } = await import("node:fs");
for (const [file, icon] of [
  ["ModelSelector.tsx", "Cpu"],
  ["LocalLlmModelSelector.tsx", "Sparkles"],
]) {
  const source = readFileSync(
    new URL(`../../components/model-selector/${file}`, import.meta.url),
    "utf8",
  );
  const picker = source.match(/<ModelStatusButton\b[\s\S]*?\n\s*\/>/u)?.[0];
  assert.ok(picker, file);
  assert.ok(
    picker.includes(`icon={<${icon} size={16} />}`),
    `${file} passes its sidebar icon`,
  );
  assert.ok(
    /<div[^>]*className="relative flex-1 min-w-0"/u.test(source),
    `${file} allocates remaining width`,
  );
}
const footerSource = readFileSync(
  new URL("../../components/footer/Footer.tsx", import.meta.url),
  "utf8",
);
assert.ok(
  footerSource.includes(
    'className="flex justify-between items-center gap-3 min-w-0 text-xs px-4 pb-3 text-text/60"',
  ),
);
assert.ok(
  /<div className="flex items-center gap-4 min-w-0 flex-1">\s*<ModelSelector \/>\s*<LocalLlmModelSelector/u.test(
    footerSource,
  ),
);
assert.ok(
  /<div className="flex items-center gap-1 shrink-0">\s*<UpdateChecker/u.test(
    footerSource,
  ),
);
console.log("picker wiring and Footer flex allocation: all assertions passed");

const { default: ModelDropdown } = await import(
  "../../components/model-selector/ModelDropdown"
);
const transcriptionModel: ModelInfo = {
  id: "size-fixture",
  name: "Distinctive transcription fixture",
  description: "Distinctive transcription description.",
  filename: "fixture.bin",
  source: { Url: { url: "https://example.com/fixture.bin", sha256: null } },
  size_mb: 1234.6,
  is_downloaded: true,
  is_downloading: false,
  partial_size: 0,
  is_directory: false,
  engine_type: "TranscribeCpp",
  accuracy_score: 0,
  speed_score: 0,
  supports_translation: false,
  is_recommended: false,
  supported_languages: ["en"],
  supports_language_selection: false,
  is_custom: false,
  supports_streaming: false,
  supports_language_detection: false,
};
const transcriptionRowsMarkup = renderToStaticMarkup(
  React.createElement(ModelDropdown, {
    models: [
      transcriptionModel,
      {
        ...transcriptionModel,
        id: "other-size-fixture",
        name: "Other transcription fixture",
        size_mb: 42.1,
      },
    ],
    currentModelId: transcriptionModel.id,
    onModelSelect: () => undefined,
  }),
);
const transcriptionRows = transcriptionRowsMarkup.match(
  /<div\b[^>]*role="button"[^>]*>[\s\S]*?(?=<div\b[^>]*role="button"|$)/gu,
);
assert.equal(transcriptionRows?.length, 2);
const transcriptionRow = transcriptionRows?.find((entry) =>
  entry.includes(transcriptionModel.name),
);
assert.ok(transcriptionRow);
const transcriptionSizeLine = transcriptionRow.match(
  /<div class="text-xs text-mid-gray">([^<]*)<\/div>/u,
);
assert.ok(transcriptionSizeLine, "transcription row has its own size line");
assert.equal(transcriptionSizeLine[1], "1235 MiB");
assert.ok(
  transcriptionRow.includes(
    `<div class="text-xs text-text/40 italic pe-4">${transcriptionModel.description}</div>${transcriptionSizeLine[0]}`,
  ),
  "transcription size immediately follows its description",
);
console.log("transcription row rounded MiB size: all assertions passed");

const downloadedRows = [
  model,
  { ...model, id: "unselected-model", display_name: "Unselected model" },
];
for (const selectedId of [model.id, null]) {
  const dropdown = renderToStaticMarkup(
    React.createElement(LocalLlmDropdown, {
      models: downloadedRows,
      selectedId,
      disabled: false,
      onSelect: () => undefined,
    }),
  );
  const buttons = dropdown.match(/<button\b[\s\S]*?<\/button>/gu);
  assert.equal(buttons?.length, downloadedRows.length);
  for (const entry of downloadedRows) {
    const ownRow: string | undefined = buttons?.find((entryRow) =>
      entryRow.includes(`title="${entry.display_name}"`),
    );
    assert.ok(ownRow, `row for ${entry.id}`);
    const activeBadges: string[] = ownRow.match(/>Active<\/span>/gu) ?? [];
    assert.equal(
      activeBadges.length,
      entry.id === selectedId ? 1 : 0,
      `Active badge only on selected row: ${entry.id}, selected=${selectedId}`,
    );
  }
}
console.log("selected and unselected local row badges: all assertions passed");
