import { z } from "zod";

export const IDENTIFIER_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;

export const identifierSchema = z.string().regex(IDENTIFIER_PATTERN);

export const lowerHexStringSchema = (bytes: number, label: string) =>
  z.string().regex(new RegExp(`^[0-9a-f]{${bytes * 2}}$`), {
    error: `${label} must be ${bytes * 2} lowercase hex characters`,
  });

export const lowerHexBytesSchema = (bytes: number, label: string) =>
  lowerHexStringSchema(bytes, label).transform(hexBytes);

export const nonNegativeSafeIntegerSchema = z
  .number()
  .int()
  .min(0)
  .max(Number.MAX_SAFE_INTEGER);

export const cursorStringSchema = z
  .string()
  .regex(/^(0|[1-9][0-9]*)$/)
  .transform(Number)
  .pipe(nonNegativeSafeIntegerSchema);

export function boundedStringSchema(label: string, maxUtf8Bytes: number) {
  const encoder = new TextEncoder();
  return z
    .string()
    .min(1, `${label} must not be empty`)
    .refine((value) => encoder.encode(value).byteLength <= maxUtf8Bytes, {
      error: `${label} exceeds ${maxUtf8Bytes} UTF-8 bytes`,
    });
}

export function hexBytes(value: string): Uint8Array {
  return Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

export function firstZodMessage(error: z.ZodError): string {
  return error.issues[0]?.message ?? "request validation failed";
}
