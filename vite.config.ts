import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import type { Plugin } from "vite";

// Attribute the packages which actually reach the built browser bundle. The
// public metadata contains upstream names/texts, never build-machine paths.
function webNotices(): Plugin {
  return {
    name: "web-third-party-notices",
    generateBundle(_, bundle) {
      const packages = new Map<string, object>();
      for (const output of Object.values(bundle)) {
        if (output.type !== "chunk") continue;
        for (const id of Object.keys(output.modules)) {
          const file = id.split("?")[0];
          if (
            !file.replaceAll("\\", "/").includes("/node_modules/") ||
            !existsSync(file)
          )
            continue;
          let directory = dirname(file);
          while (directory.replaceAll("\\", "/").includes("/node_modules/")) {
            const manifest = join(directory, "package.json");
            const pkg = existsSync(manifest)
              ? JSON.parse(readFileSync(manifest, "utf8"))
              : {};
            if (pkg.name && pkg.version) {
              const key = pkg.name + "@" + pkg.version;
              if (!packages.has(key)) {
                const notices = readdirSync(directory)
                  .filter(
                    (name) =>
                      /^(licen[sc]e|copying|copyright|notice)([._-]|$)/i.test(
                        name,
                      ) && statSync(join(directory, name)).isFile(),
                  )
                  .sort()
                  .map((name) => ({
                    file: name,
                    text: readFileSync(join(directory, name), "utf8"),
                  }));
                if (!notices.length)
                  throw Error(
                    "No license text for bundled browser package " + key,
                  );
                packages.set(key, {
                  name: pkg.name,
                  version: pkg.version,
                  license: pkg.license,
                  repository:
                    typeof pkg.repository === "string"
                      ? pkg.repository
                      : pkg.repository?.url,
                  notices,
                });
              }
              break;
            }
            directory = dirname(directory);
          }
        }
      }
      if (!packages.size) throw Error("Browser dependency inventory is empty");
      this.emitFile({
        type: "asset",
        fileName: "third-party-notices.json",
        source:
          JSON.stringify(
            {
              schemaVersion: 1,
              packages: [...packages.keys()]
                .sort()
                .map((name) => packages.get(name)),
            },
            null,
            2,
          ) + "\n",
      });
    },
  };
}
export default defineConfig({
  plugins: [vue(), webNotices()],
  root: "web",
  build: { outDir: "../dist/web", emptyOutDir: true },
  server: { host: "127.0.0.1", proxy: { "/api": "http://127.0.0.1:8765" } },
});
