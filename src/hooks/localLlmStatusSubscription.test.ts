import assert from "node:assert/strict";
import type { LocalLlmStatus } from "@/bindings";
import { subscribeLocalLlmStatus } from "./localLlmStatusSubscription";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
const initial: LocalLlmStatus = {
  state: "unloaded",
  model_id: null,
  error: null,
};
const newer: LocalLlmStatus = {
  state: "ready",
  model_id: "downloaded-b",
  error: null,
};
function setup(snapshot: Promise<LocalLlmStatus> = Promise.resolve(initial)) {
  const statuses: LocalLlmStatus[] = [];
  const errors: string[] = [];
  const calls: string[] = [];
  let event!: (status: LocalLlmStatus) => void;
  const subscription = subscribeLocalLlmStatus({
    listen: async (callback) => {
      calls.push("listen");
      event = callback;
      return () => {
        calls.push("unlisten");
      };
    },
    getStatus: () => {
      calls.push("initial");
      return snapshot;
    },
    setStatus: (status) => statuses.push(status),
    setError: (error) => errors.push(error),
    onCleanupError: (error) => {
      throw error;
    },
  });
  return {
    ...subscription,
    statuses,
    errors,
    calls,
    emit: (status: LocalLlmStatus) => event(status),
  };
}
const loaded = setup();
await loaded.ready;
assert.deepEqual(loaded.calls, ["listen", "initial"]);
assert.deepEqual(loaded.statuses, [initial]);
loaded.emit(newer);
assert.deepEqual(loaded.statuses, [initial, newer]);
loaded.stop();
await Promise.resolve();
assert.deepEqual(loaded.calls, ["listen", "initial", "unlisten"]);
loaded.emit(initial);
assert.deepEqual(
  loaded.statuses,
  [initial, newer],
  "stopped subscriptions ignore events",
);

const snapshot = deferred<LocalLlmStatus>();
const ordered = setup(snapshot.promise);
await Promise.resolve();
assert.deepEqual(
  ordered.calls,
  ["listen", "initial"],
  "initial fetch is pending after subscription",
);
ordered.emit(newer);
snapshot.resolve(initial);
await ordered.ready;
assert.deepEqual(
  ordered.statuses,
  [newer],
  "older initial snapshot must not overwrite a newer event",
);
ordered.stop();

const delayed = deferred<LocalLlmStatus>();
const stopped = setup(delayed.promise);
await Promise.resolve();
stopped.stop();
delayed.resolve(initial);
await stopped.ready;
assert.deepEqual(
  stopped.statuses,
  [],
  "cleanup also rejects late initial snapshots",
);
assert.deepEqual(stopped.calls, ["listen", "initial", "unlisten"]);

const failure = setup(Promise.reject(new Error("status fetch failed")));
await failure.ready;
assert.deepEqual(failure.errors, ["status fetch failed"]);
failure.stop();

const listening = deferred<() => void>();
const calls: string[] = [];
const errors: string[] = [];
const beforeListening = subscribeLocalLlmStatus({
  listen: () => listening.promise,
  getStatus: async () => initial,
  setStatus: () => calls.push("status"),
  setError: (error) => errors.push(error),
  onCleanupError: (error) => {
    throw error;
  },
});
beforeListening.stop();
listening.resolve(() => {
  calls.push("unlisten");
});
await beforeListening.ready;
await Promise.resolve();
assert.deepEqual(
  calls,
  ["unlisten"],
  "cleanup waits for pending subscription registration",
);
assert.equal(errors.length, 0);

const rejected = subscribeLocalLlmStatus({
  listen: async () => {
    throw new Error("listener failed");
  },
  getStatus: async () => {
    throw new Error("initial fetch must not start");
  },
  setStatus: () => {
    throw new Error("no status should be emitted");
  },
  setError: (error) => errors.push(error),
  onCleanupError: () => calls.push("cleanup error"),
});
await rejected.ready;
assert.deepEqual(errors, ["listener failed"]);
rejected.stop();
await Promise.resolve();
await Promise.resolve();
assert.deepEqual(calls, ["unlisten", "cleanup error"]);
console.log(
  "localLlmStatusSubscription: ordering and lifecycle assertions passed",
);
