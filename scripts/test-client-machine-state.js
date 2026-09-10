const assert = require("node:assert/strict");
const state = require("../apps/rustconsole-client/ui/machine-state.js");

const machine = {
  availability: "busy",
  availabilityDetail: "Busy: another player is streaming",
  endpoints: ["host:47999"],
};
const idle = { phase: "idle", endpoint: null };

assert.deepEqual(state.presentation(machine, idle), {
  state: "busy",
  detail: "Busy: another player is streaming",
});
assert.deepEqual(state.action(machine, idle), {
  kind: "probe",
  label: "Check again",
  disabled: false,
});

const launching = { phase: "launching", endpoint: "host:47999" };
assert.deepEqual(state.presentation(machine, launching), {
  state: "checking",
  detail: "Launching player",
});
assert.equal(state.action(machine, launching).disabled, true);

const authenticated = { phase: "authenticated", endpoint: "host:47999" };
assert.equal(state.presentation(machine, authenticated).detail, "Authenticated");
const streaming = { phase: "streaming", endpoint: "host:47999" };
assert.equal(state.presentation(machine, streaming).detail, "Player streaming");
const stopping = { phase: "stopping", endpoint: "host:47999" };
assert.equal(state.presentation(machine, stopping).detail, "Disconnecting");

const otherPlayer = { phase: "streaming", endpoint: "other:47999" };
assert.equal(state.presentation(machine, otherPlayer).state, "busy");
assert.equal(state.action(machine, otherPlayer).disabled, true);

const failedMachine = { ...machine, availability: "unreachable", availabilityDetail: "Could not reach machine" };
assert.equal(state.action(failedMachine, idle).label, "Retry");
