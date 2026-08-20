import { DurableObject } from "cloudflare:workers";
import type { RepositoryAuthority } from "./control_plane";
import { Kernel, equalGitBytes, exactGitBuffer } from "./kernel";
import { OpGitStore } from "./op_store";
import { ProjectionGitStore } from "./projection_store";
import { initializeGitSchema } from "./schema";
import { hexBytes } from "./validation";

class RepositoryAuthorityError extends Error {
  constructor(
    message: string,
    readonly code: "repository-authority-stale",
  ) {
    super(message);
  }
}

interface AuthorityRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  user_id: string;
  repository_id: string;
  creation_nonce: string | null;
}

interface RepositoryRetirement {
  authority: RepositoryAuthority;
  completion: Promise<void>;
}

export class Repository extends DurableObject<Env> {
  private readonly ops: OpGitStore;
  private readonly projection: ProjectionGitStore;
  private retirement: RepositoryRetirement | undefined;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    const sql = this.ctx.storage.sql;
    this.ctx.blockConcurrencyWhile(async () =>
      this.ctx.storage.transactionSync(() => initializeGitSchema(sql)),
    );
    const kernel = new Kernel();
    this.ops = new OpGitStore(this.ctx, sql, kernel);
    this.projection = new ProjectionGitStore(this.ctx, sql, kernel);
  }

  async initializeRepository(authority: RepositoryAuthority) {
    try {
      const existing = this.authorityState();
      if (existing !== undefined) {
        this.requireAuthority(authority);
        return { ok: true as const, initialized: false };
      }
      const control = this.env.CONTROL_PLANE.getByName("directory");
      const allowed = await control.validateRepositoryInitialization(authority);
      if (!allowed.ok) {
        throw new RepositoryAuthorityError(
          "repository authority is stale",
          "repository-authority-stale",
        );
      }
      const initialized = this.ctx.storage.transactionSync(() => {
        const state = this.authorityState();
        if (state !== undefined) {
          this.requireAuthority(authority);
          return false;
        }
        this.ctx.storage.sql.exec(
          `INSERT INTO repository_state
           (singleton, incarnation, user_id, repository_id, creation_nonce)
           VALUES (1, ?, ?, ?, ?)`,
          exactGitBuffer(incarnationBytes(authority.incarnation)),
          authority.userId,
          authority.repositoryId,
          authority.creationNonce,
        );
        return true;
      });
      const confirmed = await control.validateRepositoryInitialization(authority);
      if (!confirmed.ok) {
        await this.ctx.storage.deleteAll();
        throw new RepositoryAuthorityError(
          "repository authority is stale",
          "repository-authority-stale",
        );
      }
      return { ok: true as const, initialized };
    } catch (error) {
      return authorityFailure(error);
    }
  }

  async retireRepository(authority: RepositoryAuthority) {
    const retirement = this.retirement;
    if (retirement !== undefined) {
      if (!sameRepositoryAuthority(retirement.authority, authority)) {
        return authorityFailure(
          new RepositoryAuthorityError(
            "repository authority is stale",
            "repository-authority-stale",
          ),
        );
      }
      await retirement.completion;
      return { ok: true as const, retired: true };
    }

    try {
      const state = this.authorityState();
      if (state !== undefined) this.requireAuthority(authority);
      const completion = this.ctx.storage.deleteAll();
      this.retirement = { authority: { ...authority }, completion };
      try {
        await completion;
      } catch (error) {
        if (this.retirement?.completion === completion) this.retirement = undefined;
        throw error;
      }
      return { ok: true as const, retired: true };
    } catch (error) {
      return authorityFailure(error);
    }
  }

  putOpObject(
    authority: RepositoryAuthority,
    kind: "view" | "operation",
    id: string,
    bytes: Uint8Array,
  ) {
    return this.withAuthority(authority, () => this.ops.put(kind, id, bytes));
  }

  getOpObject(authority: RepositoryAuthority, kind: "view" | "operation", id: string) {
    return this.withAuthority(authority, () => this.ops.get(kind, id));
  }

  inventoryOpObjects(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.ops.inventory(value));
  }

  getOpHeads(authority: RepositoryAuthority) {
    return this.withAuthority(authority, () => this.ops.getHeads(authority.incarnation));
  }

  transactOpHeads(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.ops.transactHeads(value));
  }

  getProjection(
    authority: RepositoryAuthority,
    incarnationValue: unknown,
    afterValue: unknown,
    throughValue: unknown,
  ) {
    return this.withAuthority(authority, () =>
      this.projection.get(incarnationValue, afterValue, throughValue),
    );
  }

  setRemote(authority: RepositoryAuthority, name: unknown, value: unknown) {
    return this.withAuthority(authority, () => this.projection.setRemote(name, value));
  }

  listRemotes(authority: RepositoryAuthority, incarnationValue: unknown) {
    return this.withAuthority(authority, () => this.projection.listRemotes(incarnationValue));
  }

  beginProjectionPush(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.projection.begin(value, authority.machineId));
  }

  recordProjectionFetch(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.recordFetch(value, authority.machineId),
    );
  }

  claimProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.claim(batchId, value, authority.machineId),
    );
  }

  getProjectionPushReplay(
    authority: RepositoryAuthority,
    batchId: unknown,
    incarnationValue: unknown,
  ) {
    return this.withAuthority(authority, () =>
      this.projection.replay(batchId, incarnationValue),
    );
  }

  recoverProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.recover(batchId, value, authority.machineId),
    );
  }

  private withAuthority<T>(authority: RepositoryAuthority, operation: () => T) {
    try {
      this.requireAuthority(authority);
    } catch (error) {
      return authorityFailure(error);
    }
    return operation();
  }

  private authorityState(): AuthorityRow | undefined {
    return this.ctx.storage.sql
      .exec<AuthorityRow>(
        `SELECT incarnation, user_id, repository_id, creation_nonce
         FROM repository_state WHERE singleton = 1`,
      )
      .toArray()[0];
  }

  private requireAuthority(authority: RepositoryAuthority) {
    const state = this.authorityState();
    if (
      state === undefined ||
      state.user_id !== authority.userId ||
      state.repository_id !== authority.repositoryId ||
      state.creation_nonce !== authority.creationNonce ||
      !equalGitBytes(new Uint8Array(state.incarnation), incarnationBytes(authority.incarnation))
    ) {
      throw new RepositoryAuthorityError(
        "repository authority is stale",
        "repository-authority-stale",
      );
    }
  }
}

function authorityFailure(error: unknown) {
  if (!(error instanceof RepositoryAuthorityError)) throw error;
  return {
    ok: false as const,
    status: 409,
    error: error.message,
    code: error.code,
  };
}

function sameRepositoryAuthority(left: RepositoryAuthority, right: RepositoryAuthority): boolean {
  return (
    left.userId === right.userId &&
    left.repositoryId === right.repositoryId &&
    left.incarnation === right.incarnation &&
    left.creationNonce === right.creationNonce
  );
}

function incarnationBytes(value: string): Uint8Array {
  if (!/^[0-9a-f]{32}$/.test(value)) {
    throw new RepositoryAuthorityError(
      "repository authority is stale",
      "repository-authority-stale",
    );
  }
  return hexBytes(value);
}
