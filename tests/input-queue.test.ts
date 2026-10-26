// Existing browser queue regressions retained from b40b2dd against current Vue source.
import { test } from "node:test";
import assert from "node:assert/strict";
import { InputQueue } from "../web/input-queue.js";

test("input queue is bounded, merges scrolls and cancels pending input", async () => {
  let finish!: () => void;
  const calls: unknown[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    await new Promise<void>((resolve) => {
      finish = resolve;
    });
  }, 2);
  const active = queue.enqueue([{ type: "keypress", keys: ["KeyA"] }]);
  const scroll = {
    type: "scroll" as const,
    x: 1,
    y: 1,
    scroll_x: 0,
    scroll_y: 100,
  };
  const a = queue.enqueue([scroll]),
    b = queue.enqueue([scroll]);
  const c = queue.enqueue([{ type: "keypress", keys: ["KeyB"] }]);
  assert.equal(queue.pending, 2);
  assert.equal(
    await queue.enqueue([{ type: "keypress", keys: ["KeyC"] }]),
    false,
  );
  queue.cancel();
  finish();
  assert.deepEqual(await Promise.all([active, a, b, c]), [
    true,
    false,
    false,
    false,
  ]);
  assert.equal(calls.length, 1);
});

test("input queue executes merged scroll distance once", async () => {
  let finish!: () => void;
  const calls: any[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    if (calls.length === 1)
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
  });
  const active = queue.enqueue([{ type: "keypress", keys: ["KeyA"] }]);
  const scroll = {
    type: "scroll" as const,
    x: 1,
    y: 1,
    scroll_x: 0,
    scroll_y: 100,
  };
  const a = queue.enqueue([scroll]),
    b = queue.enqueue([scroll]);
  finish();
  assert.deepEqual(await Promise.all([active, a, b]), [true, true, true]);
  assert.equal(calls.length, 2);
  assert.equal(calls[1][0].scroll_y, 200);
});

test("mouse movement coalesces only across adjacent moves and preserves clicks", async () => {
  let finish!: () => void;
  const calls: any[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    if (calls.length === 1)
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
  });
  const pending = [queue.enqueue([{ type: "move", x: 0, y: 0 }])];
  pending.push(queue.enqueue([{ type: "move", x: 1, y: 1 }]));
  pending.push(queue.enqueue([{ type: "move", x: 9, y: 9 }]));
  pending.push(queue.enqueue([{ type: "click", x: 9, y: 9, button: "left" }]));
  pending.push(queue.enqueue([{ type: "move", x: 10, y: 10 }]));
  finish();
  assert.deepEqual(await Promise.all(pending), [true, true, true, true, true]);
  assert.deepEqual(
    calls.map((a) => a[0].type),
    ["move", "move", "click", "move"],
  );
  assert.equal(calls[1][0].x, 9);
});

test("input queue reports rejected and skipped execution without claiming delivery", async () => {
  const skipped = new InputQueue(async () => false);
  assert.equal(
    await skipped.enqueue([{ type: "type", text: "preserve draft" }]),
    false,
  );
  const failed = new InputQueue(async () => {
    throw Error("offline");
  });
  assert.equal(
    await failed.enqueue([{ type: "type", text: "preserve draft" }]),
    false,
  );
});
