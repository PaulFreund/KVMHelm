import type { ComputerAction } from "../shared/contracts";
type Entry = { actions: ComputerAction[]; done: ((ok: boolean) => void)[] };
/** Bounded pending input; cancellation never retries the active operation. */
export class InputQueue {
  private items: Entry[] = [];
  private running = false;
  constructor(
    private execute: (actions: ComputerAction[]) => Promise<void>,
    readonly capacity = 32,
  ) {}
  get pending() {
    return this.items.length;
  }
  cancel() {
    for (const item of this.items.splice(0))
      item.done.forEach((done) => done(false));
  }
  enqueue(actions: ComputerAction[]): Promise<boolean> {
    const last = this.items.at(-1),
      a = actions[0],
      b = last?.actions[0];
    if (
      actions.length === 1 &&
      last?.actions.length === 1 &&
      a.type === "scroll" &&
      b?.type === "scroll" &&
      a.x === b.x &&
      a.y === b.y &&
      Math.abs(a.scroll_x + b.scroll_x) <= 1000 &&
      Math.abs(a.scroll_y + b.scroll_y) <= 1000
    ) {
      b.scroll_x += a.scroll_x;
      b.scroll_y += a.scroll_y;
      return new Promise((done) => last.done.push(done));
    }
    if (this.items.length >= this.capacity) return Promise.resolve(false);
    return new Promise((done) => {
      this.items.push({ actions: structuredClone(actions), done: [done] });
      void this.drain();
    });
  }
  private async drain() {
    if (this.running) return;
    this.running = true;
    try {
      while (this.items.length) {
        const item = this.items.shift()!;
        try {
          await this.execute(item.actions);
          item.done.forEach((done) => done(true));
        } catch {
          item.done.forEach((done) => done(false));
          this.cancel();
        }
      }
    } finally {
      this.running = false;
    }
  }
}
