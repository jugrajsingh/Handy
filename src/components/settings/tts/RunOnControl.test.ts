import assert from "node:assert/strict";
import { applyRunOnChoice, type RunOnControlProps } from "./RunOnControl";
import type { ModelRating, TtsRunOn } from "./presentation";
const supported = {
  supported: true,
  recommended: false,
  speed_score: 5,
  first_audio_p95_s: 0.5,
  rtf_1x: 0.2,
  rtf_2x: 0.3,
  load_s_cold: 2,
  load_s_cached: 1,
  placement: { graph: "cpu" as const },
};
const rating: ModelRating = {
  quality: 8,
  devices: {
    cpu: supported,
    gpu: { ...supported, supported: false },
    npu: supported,
  },
};
const choices: TtsRunOn[] = [];
const input: Pick<
  RunOnControlProps,
  "downloaded" | "invalidNpu" | "available" | "rating" | "change"
> = {
  downloaded: true,
  invalidNpu: false,
  available: ["cpu", "gpu", "npu"],
  rating,
  change: (next) => choices.push(next),
};
assert.equal(applyRunOnChoice(input, "auto"), true);
assert.equal(applyRunOnChoice(input, "npu"), true);
assert.deepEqual(choices, ["auto", "npu"]);
for (const [state, next] of [
  [{ ...input, downloaded: false }, "cpu"],
  [{ ...input, invalidNpu: true }, "npu"],
  [{ ...input, available: ["cpu"] as const }, "npu"],
  [input, "gpu"],
  [input, "invalid"],
] as const) {
  const accepted = applyRunOnChoice(state, next);
  assert.equal(choices.length, 2, `refused ${next} callback count`);
  assert.equal(accepted, false);
}
assert.deepEqual(choices, ["auto", "npu"]);
console.log(
  "Run on action seam: download/driver/visibility/support/unknown guards passed",
);
