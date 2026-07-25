import { z } from "zod";

import { compareGitBytes } from "./kernel";
import { lowerHexBytesSchema } from "./validation";

export const MAX_OPERATION_HEADS = 4_096;
export const MAX_OP_INVENTORY_KEYS = 4_096;
export const MAX_HEAD_REQUEST_BYTES = 640 * 1_024;

const INVENTORY_BOUNDS_ERROR =
  "operation-store inventory request must contain only a bounded keys array";

const shortHex = (label: string) => lowerHexBytesSchema(16, label);
const operationId = (label: string) => lowerHexBytesSchema(64, label);

const headTransactionSchema = z
  .strictObject({
    incarnation: shortHex("incarnation"),
    idempotencyKey: shortHex("idempotencyKey"),
    newHead: operationId("newHead").refine((value) => value.some((byte) => byte !== 0), {
      error: "newHead must not be the implicit zero operation",
    }),
    observedHeads: z.array(operationId("observed head")).max(MAX_OPERATION_HEADS),
  })
  .transform((request, context) => {
    request.observedHeads.sort(compareGitBytes);
    for (let index = 1; index < request.observedHeads.length; index += 1) {
      if (compareGitBytes(request.observedHeads[index - 1], request.observedHeads[index]) === 0) {
        context.addIssue({
          code: "custom",
          path: ["observedHeads", index],
          message: "observedHeads must not contain duplicates",
        });
      }
    }
    return request;
  });

export const opInventorySchema = z
  .strictObject(
    {
      keys: z
        .array(
          z.string().regex(/^[vo]:[0-9a-f]{128}$/, {
            error: "operation-store inventory key is invalid",
          }),
          { error: INVENTORY_BOUNDS_ERROR },
        )
        .max(MAX_OP_INVENTORY_KEYS, { error: INVENTORY_BOUNDS_ERROR }),
    },
    { error: INVENTORY_BOUNDS_ERROR },
  )
  .superRefine((request, context) => {
    for (let index = 1; index < request.keys.length; index += 1) {
      if (request.keys[index - 1] >= request.keys[index]) {
        context.addIssue({
          code: "custom",
          path: ["keys", index],
          message: "operation-store inventory keys must be strictly sorted",
        });
      }
    }
  });

export type HeadTransactionRequest = z.output<typeof headTransactionSchema>;

export function decodeHeadTransaction(value: unknown): HeadTransactionRequest {
  return headTransactionSchema.parse(value);
}

export function canonicalHeadTransactionBytes(request: HeadTransactionRequest): Uint8Array {
  const bytes = new Uint8Array(8 + 16 + 64 + request.observedHeads.length * 64);
  const view = new DataView(bytes.buffer);
  bytes.set(new TextEncoder().encode("DSHD"));
  view.setUint16(4, 1, true);
  view.setUint16(6, request.observedHeads.length, true);
  bytes.set(request.incarnation, 8);
  bytes.set(request.newHead, 24);
  for (const [index, head] of request.observedHeads.entries()) {
    bytes.set(head, 88 + index * 64);
  }
  return bytes;
}
