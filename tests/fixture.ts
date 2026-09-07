import type { TestContext } from "node:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { Store, Secrets } from "../server/store.js";
import { Auth } from "../server/auth.js";
import { Core } from "../server/core.js";
import { PluginHost } from "../server/plugins.js";
import { scopes } from "../shared/contracts.js";

export async function fixture(t: TestContext) {
  const dir = await mkdtemp(join(tmpdir(), "kvmhelm-regression-"));
  const store = new Store(dir),
    secrets = await Secrets.open(join(dir, "secrets"));
  const auth = new Auth(store),
    core = new Core(store, secrets, auth);
  const plugins = new PluginHost(core, join(dir, "plugins"));
  t.after(async () => {
    await plugins.shutdown();
    await core.shutdown();
    await store.close();
    const target = resolve(dir),
      prefix = resolve(tmpdir()) + sep;
    if (
      !target.startsWith(prefix) ||
      !target.slice(prefix.length).startsWith("kvmhelm-regression-")
    )
      throw Error("Unsafe cleanup");
    await rm(target, { recursive: true, force: true });
  });
  const token = await auth.create({
    name: "fixture",
    scopes: [...scopes],
    devices: ["*"],
  });
  const identity = auth.authenticate(token.token, "regression");
  const device = await core.saveDevice(identity, {
    name: "Fixture",
    driver_id: "simulator",
    transport: "simulator",
  });
  return { dir, store, secrets, auth, core, plugins, token, identity, device };
}
