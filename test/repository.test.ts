import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import gitGolden from "../crates/kernel/tests/git_golden.txt?raw";
import { Kernel, gitToHex } from "../src/kernel";
import {
  countRows,
  decodeHex,
  ensureRepository,
  json,
  repositoryGitStub,
  routeRequest,
} from "./support";

describe("Git validation kernel", () => {
  it("rejects truncated and mutated real Git vectors", () => {
    const lines = gitGolden
      .split("\n")
      .filter((line) => line.startsWith("tree|") || line.startsWith("commit|"));
    const tree = decodeHex(
      lines.find((line) => line.startsWith("tree|") && line.split("|")[2] !== "")!.split("|")[2],
    );
    const commit = decodeHex(lines.find((line) => line.startsWith("commit|"))!.split("|")[2]);
    const headerTerminator = findSequence(commit, new Uint8Array([0x0a, 0x0a]));
    const mutatedCommit = commit.slice();
    mutatedCommit[0] = 0;

    const kernel = new Kernel();
    expect(() => kernel.validate(1, tree.slice(0, -1))).toThrow();
    expect(() => kernel.validate(2, commit.slice(0, headerTerminator + 1))).toThrow();
    expect(() => kernel.validate(2, mutatedCommit)).toThrow();
  });
});

describe("Git operation store and heads", () => {
  it("rejects noncanonical objects and converges concurrent heads idempotently", async () => {
    const name = "git-ops-convergence";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: true });
    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: false });

    const noncanonical = Uint8Array.from([...view, 0x68, 0x01]);
    const rejected = await putOp(name, "views", viewId, noncanonical);
    expect(rejected.status).toBe(400);
    expect(await rejected.json()).toMatchObject({
      error: expect.stringContaining("does not exactly re-encode"),
    });

    const base = await installOperation(name, viewId, "base");
    const left = await installOperation(name, viewId, "left", [base]);
    const right = await installOperation(name, viewId, "right", [base]);
    const merged = await installOperation(name, viewId, "merged", [left, right]);
    const incarnation = (await ensureRepository(name)).incarnation;

    expect(await postOpHeads(name, incarnation, "01".repeat(16), base, [])).toEqual({
      cursor: 1,
      heads: [base],
    });
    const leftRequest = opHeadRequest(incarnation, "02".repeat(16), left, [base]);
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(leftRequest),
    }))).toEqual({ cursor: 2, heads: [left] });
    expect(await postOpHeads(name, incarnation, "03".repeat(16), right, [base])).toEqual({
      cursor: 3,
      heads: [left, right].sort(),
    });
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(leftRequest),
    }))).toEqual({ cursor: 2, heads: [left] });
    expect(await postOpHeads(name, incarnation, "04".repeat(16), merged, [right, left])).toEqual({
      cursor: 4,
      heads: [merged],
    });
  });

  it("does not consume a head receipt until the complete op closure exists", async () => {
    const name = "git-ops-incomplete";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    const operation = await installOperation(name, viewId, "needs view");
    const incarnation = (await ensureRepository(name)).incarnation;
    const request = opHeadRequest(incarnation, "0a".repeat(16), operation, []);

    const incomplete = await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(request),
    });
    expect(incomplete.status).toBe(409);
    expect(await incomplete.json()).toMatchObject({ code: "head-closure-incomplete" });

    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: true });
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(request),
    }))).toEqual({ cursor: 1, heads: [operation] });
  });

  it("persists operation objects, exact bytes, heads and receipts across eviction", async () => {
    const name = "git-ops-eviction";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    await putOp(name, "views", viewId, view);
    const operation = await installOperation(name, viewId, "persistent");
    const incarnation = (await ensureRepository(name)).incarnation;
    expect(await postOpHeads(name, incarnation, "11".repeat(16), operation, [])).toEqual({
      cursor: 1,
      heads: [operation],
    });

    await evictDurableObject(await repositoryGitStub(name));
    const downloaded = await routeRequest(name, `git/ops/operations/${operation}`, {
      method: "GET",
    });
    expect(downloaded.status).toBe(200);
    expect(new Uint8Array(await downloaded.arrayBuffer())).toEqual(
      canonicalGitOperation(decodeHex(viewId), "persistent"),
    );
    expect(await json(await routeRequest(name, "git/ops/heads", { method: "GET" }))).toEqual({
      cursor: 1,
      heads: [operation],
    });
    expect(await postOpHeads(name, incarnation, "11".repeat(16), operation, [])).toEqual({
      cursor: 1,
      heads: [operation],
    });
    expect(await countRows(await repositoryGitStub(name), "op_objects")).toBe(2);
  });

  it("keeps validation public but sanitizes unexpected operation storage failures", async () => {
    const name = "git-ops-storage-failure";
    const invalid = await routeRequest(name, "git/ops/inventory", {
      method: "POST",
      body: JSON.stringify({ keys: ["not-an-operation-key"] }),
    });
    expect(invalid.status).toBe(400);
    expect(await invalid.json()).toEqual({
      error: "operation-store inventory key is invalid",
      code: "invalid-op-request",
    });

    await runInDurableObject(await repositoryGitStub(name), (_instance, state) => {
      state.storage.sql.exec("DROP TABLE op_objects");
    });
    const response = await routeRequest(name, "git/ops/inventory", {
      method: "POST",
      body: JSON.stringify({ keys: [`v:${"01".repeat(64)}`] }),
    });
    expect(response.status).toBe(500);
    expect(await response.json()).toEqual({ error: "Git repository storage failed" });
  });
});

function putOp(
  name: string,
  kind: "views" | "operations",
  id: string,
  bytes: Uint8Array,
) {
  return routeRequest(name, `git/ops/${kind}/${id}`, { method: "PUT", body: bytes });
}

async function installOperation(
  name: string,
  viewId: string,
  description: string,
  parents = ["00".repeat(64)],
): Promise<string> {
  const bytes = canonicalGitOperation(decodeHex(viewId), description, parents.map(decodeHex));
  const id = gitToHex(new Kernel().validateOperation(bytes).id);
  expect(await json(await putOp(name, "operations", id, bytes))).toMatchObject({
    inserted: expect.any(Boolean),
  });
  return id;
}

async function postOpHeads(
  name: string,
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return json(await routeRequest(name, "git/ops/heads/transactions", {
    method: "POST",
    body: JSON.stringify(opHeadRequest(incarnation, idempotencyKey, newHead, observedHeads)),
  }));
}

function opHeadRequest(
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return { incarnation, idempotencyKey, newHead, observedHeads };
}

function canonicalGitView(): Uint8Array {
  return new Uint8Array([
    0x0a,
    20,
    ...new Uint8Array(20),
    0x4a,
    4,
    0x1a,
    2,
    0x12,
    0,
    0x60,
    1,
  ]);
}

function canonicalGitOperation(
  viewId: Uint8Array,
  description: string,
  parents: Uint8Array[] = [new Uint8Array(64)],
): Uint8Array {
  const metadata: number[] = [0x0a, 0, 0x12, 0];
  pushProtoBytes(metadata, 3, new TextEncoder().encode(description));
  const operation: number[] = [];
  pushProtoBytes(operation, 1, viewId);
  for (const parent of parents) pushProtoBytes(operation, 2, parent);
  pushProtoBytes(operation, 3, Uint8Array.from(metadata));
  return Uint8Array.from(operation);
}

function pushProtoBytes(output: number[], tag: number, bytes: Uint8Array) {
  output.push((tag << 3) | 2);
  let length = bytes.byteLength;
  while (length >= 0x80) {
    output.push((length & 0x7f) | 0x80);
    length >>= 7;
  }
  output.push(length);
  output.push(...bytes);
}

function findSequence(bytes: Uint8Array, sequence: Uint8Array): number {
  for (let index = 0; index <= bytes.byteLength - sequence.byteLength; index += 1) {
    if (sequence.every((byte, offset) => bytes[index + offset] === byte)) return index;
  }
  throw new Error(`sequence not found in ${gitToHex(bytes)}`);
}
