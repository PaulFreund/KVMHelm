import { parentPort, workerData } from "node:worker_threads";
import { DatabaseSync } from "node:sqlite";
const db = new DatabaseSync(workerData.path);
const version = Number(
  db.prepare("PRAGMA user_version").get()?.user_version ?? 0,
);
if (version > 1)
  throw new Error("Database schema is newer than this KVMHelm version");
db.exec(
  "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=3000; CREATE TABLE IF NOT EXISTS documents (kind TEXT NOT NULL,id TEXT NOT NULL,json TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(kind,id)); PRAGMA user_version=1",
);
parentPort!.on("message", ({ id, op, kind, key, value, revision }) => {
  try {
    let result: unknown;
    if (op === "list")
      result = db
        .prepare("SELECT json,revision FROM documents WHERE kind=?")
        .all(kind)
        .map((r) => ({ ...JSON.parse(String(r.json)), revision: r.revision }));
    if (op === "get") {
      const r = db
        .prepare("SELECT json,revision FROM documents WHERE kind=? AND id=?")
        .get(kind, key);
      result = r
        ? { ...JSON.parse(String(r.json)), revision: r.revision }
        : null;
    }
    if (op === "put") {
      db.exec("BEGIN IMMEDIATE");
      try {
        const r = db
          .prepare("SELECT revision FROM documents WHERE kind=? AND id=?")
          .get(kind, key);
        if (Number(r?.revision ?? 0) !== revision)
          throw Error("REVISION_CONFLICT");
        const next = revision + 1;
        db.prepare(
          "INSERT INTO documents VALUES(?,?,?,?) ON CONFLICT(kind,id) DO UPDATE SET json=excluded.json,revision=excluded.revision",
        ).run(kind, key, JSON.stringify(value), next);
        db.exec("COMMIT");
        result = { ...value, revision: next };
      } catch (e) {
        db.exec("ROLLBACK");
        throw e;
      }
    }
    if (op === "delete") {
      const r = db
        .prepare("DELETE FROM documents WHERE kind=? AND id=? AND revision=?")
        .run(kind, key, revision);
      if (!r.changes) throw Error("REVISION_CONFLICT");
      result = true;
    }
    parentPort!.postMessage({ id, result });
  } catch (e) {
    parentPort!.postMessage({
      id,
      error:
        (e as Error).message === "REVISION_CONFLICT"
          ? "REVISION_CONFLICT"
          : "STORAGE_ERROR",
    });
  }
});
