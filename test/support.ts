import { env, exports } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { expect } from "vitest";
import { hexBytes } from "../src/validation";

export const DEFAULT_MACHINE = "a6".repeat(16);

export interface CloudRepository {
  repositoryId: string;
  incarnation: string;
}

type RepositoryStub = ReturnType<typeof env.REPOSITORIES.getByName>;

const repositories = new Map<string, CloudRepository>();

export function authorizationFor(machineId: string): Record<string, string> {
  return {
    authorization: `Bearer ${env.DEVSPACE_SHARED_SECRET}`,
    "x-devspace-machine-id": machineId,
  };
}

export function workerRequest(machineId: string, path: string, init: RequestInit = {}) {
  return exports.default.fetch(
    new Request(`https://example.com${path}`, {
      ...init,
      headers: { ...authorizationFor(machineId), ...init.headers },
    }),
  );
}

export async function ensureRepository(name: string): Promise<CloudRepository> {
  const existing = repositories.get(name);
  if (existing !== undefined) return existing;
  const response = await workerRequest(DEFAULT_MACHINE, "/repositories", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, idempotencyKey: randomHex(16) }),
  });
  if (!response.ok) throw new Error(`failed to create repository: ${await response.text()}`);
  const repository = (await response.json()) as CloudRepository;
  repositories.set(name, repository);
  return repository;
}

export async function routeRequest(
  name: string,
  path: string,
  init: RequestInit,
  machineId = DEFAULT_MACHINE,
) {
  const repository = await ensureRepository(name);
  return workerRequest(machineId, `/repositories/${repository.repositoryId}/${path}`, {
    ...init,
    headers: { "x-devspace-incarnation": repository.incarnation, ...init.headers },
  });
}

export async function repositoryGitStub(name: string): Promise<RepositoryStub> {
  const repository = await ensureRepository(name);
  return env.REPOSITORIES.getByName(repository.repositoryId);
}

/**
 * Reads a row count straight out of the Durable Object's storage. Rejected
 * installs must leave zero rows behind, and no client-visible route reports
 * the internal tables, so the assertion reads them instead of an RPC method
 * that would otherwise exist on the production class for tests alone.
 */
export function countRows(stub: RepositoryStub, table: string): Promise<number> {
  return runInDurableObject(stub, (_instance, state) =>
    state.storage.sql.exec<{ count: number }>(`SELECT count(*) AS count FROM ${table}`).one().count,
  );
}

export function randomHex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

export function decodeHex(value: string): Uint8Array {
  if (value.length % 2 !== 0) throw new Error("odd-length hex fixture");
  return hexBytes(value);
}

export async function json(response: Response): Promise<unknown> {
  expect(response.status).toBe(200);
  return response.json();
}
