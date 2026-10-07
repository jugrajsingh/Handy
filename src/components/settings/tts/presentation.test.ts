import assert from "node:assert/strict";
import {
  barFill,
  deviceRows,
  runOnChoices,
  type DeviceRating,
  type ModelRating,
} from "./presentation";
const supported: DeviceRating = {
  supported: true,
  placement: { main: "cpu", decoder: "npu" },
  first_audio_p95_s: 0.5,
  rtf_1x: 0.25,
  rtf_2x: 0.4,
  load_s_cold: 20,
  load_s_cached: 2,
  speed_score: 8,
  recommended: true,
};
const rating: ModelRating = {
  quality: 9,
  devices: {
    cpu: { ...supported, recommended: false },
    gpu: { ...supported, supported: false, recommended: false },
    npu: supported,
  },
};
assert.equal(barFill(9), 90);
assert.equal(barFill(12), 100);
assert.equal(barFill(-1), 0);
assert.equal(barFill(Number.NaN), 0);
assert.equal(barFill(null), 0);
assert.deepEqual(runOnChoices(["cpu", "npu"]), ["auto", "cpu", "npu"]);
const rows = deviceRows(rating, ["cpu", "npu"], false);
assert.deepEqual(
  rows.map((row) => row.device),
  ["cpu", "npu"],
);
assert.equal(rows[1].fill, 80);
assert.equal(rows[1].recommended, true);
assert.deepEqual(rows[1].tooltip, {
  firstAudio: "0.5",
  faster: "4.0",
  load: "2.0",
});
assert.deepEqual(deviceRows(rating, ["gpu"], false), []);
assert.deepEqual(deviceRows(rating, ["npu"], true), []);
const precise = {
  ...supported,
  first_audio_p95_s: 1.987654321,
  rtf_1x: 0.263157894,
  load_s_cached: 0.456789,
};
assert.deepEqual(
  deviceRows(
    { ...rating, devices: { ...rating.devices, cpu: precise } },
    ["cpu"],
    false,
  )[0].tooltip,
  { firstAudio: "2.0", faster: "3.8", load: "0.5" },
);
console.log("TTS rating presentation assertions passed");
