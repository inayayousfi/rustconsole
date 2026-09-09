const assert = require("node:assert/strict");
const { UniqueAsyncQueue } = require("../apps/rustconsole-client/ui/discovery-auth-queue.js");

async function main() {
  const queue = new UniqueAsyncQueue();
  queue.reset(7);
  assert.equal(queue.enqueue(7, "one", "first"), true);
  assert.equal(queue.enqueue(7, "one", "duplicate"), false);
  assert.equal(queue.enqueue(7, "two", "second"), true);
  assert.equal(queue.enqueue(6, "old", "stale"), false);

  const handled = [];
  await queue.drain(async (value, generation) => {
    handled.push([generation, value]);
  });
  assert.deepEqual(handled, [[7, "first"], [7, "second"]]);

  queue.reset(8);
  assert.equal(queue.isCurrent(7), false);
  assert.equal(queue.isCurrent(8), true);

  let releaseOld;
  let releaseCurrent;
  const oldBlocked = new Promise((resolve) => { releaseOld = resolve; });
  const currentBlocked = new Promise((resolve) => { releaseCurrent = resolve; });
  const started = [];
  assert.equal(queue.enqueue(8, "same", "old"), true);
  const draining = queue.drain(async (value) => {
    started.push(value);
    await (value === "old" ? oldBlocked : currentBlocked);
  });
  await new Promise((resolve) => setImmediate(resolve));
  queue.reset(9);
  assert.equal(queue.enqueue(9, "same", "current"), true);
  releaseOld();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(started, ["old", "current"]);
  assert.equal(queue.enqueue(9, "same", "duplicate-current"), false);
  releaseCurrent();
  await draining;
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
