import { expect, it } from "vitest";
import gitGolden from "../crates/kernel/tests/git_golden.txt?raw";
import gitGoldenOracle from "../crates/kernel/tests/git_golden_oracle.txt?raw";
import opsGolden from "../crates/kernel/tests/ops_golden.txt?raw";
import { GIT_OBJECT_KIND, Kernel, OP_REFERENCE_KIND, gitToHex } from "../src/kernel";
import { hexBytes } from "../src/validation";

it("matches native Git IDs and acceptance for all 40 vectors through Wasm", () => {
  const kernel = new Kernel();
  const lines = `${gitGolden}\n${gitGoldenOracle}`
    .split("\n")
    .filter((line) => line !== "" && !line.startsWith("#"));
  expect(lines).toHaveLength(40);

  for (const line of lines) {
    const [kindName, expectedId, payloadHex] = line.split("|");
    if (!(kindName in GIT_OBJECT_KIND)) throw new Error(`unknown Git object kind ${kindName}`);
    const kind = GIT_OBJECT_KIND[kindName as keyof typeof GIT_OBJECT_KIND];
    const validated = kernel.validate(kind, hexBytes(payloadHex));
    expect(gitToHex(validated.id), `${kindName} ID`).toBe(expectedId);
  }
});

it("matches ported operation IDs and enforces 20-byte Git view references through Wasm", () => {
  const kernel = new Kernel();
  const operations = opsGolden
    .split("\n")
    .filter((line) => line !== "" && !line.startsWith("#"));
  expect(operations).toHaveLength(8);
  for (const line of operations) {
    const [kind, expectedId, payloadRle] = line.split("|");
    const bytes = decodeRle(payloadRle);
    const validated =
      kind === "view"
        ? kernel.validateView(bytes)
        : kind === "operation"
          ? kernel.validateOperation(bytes)
          : undefined;
    if (validated === undefined) throw new Error(`unknown operation-store kind ${kind}`);
    expect(gitToHex(validated.id)).toBe(expectedId);
  }

  const view = new Uint8Array([
    0x0a,
    20,
    ...new Uint8Array(20).fill(1),
    0x4a,
    4,
    0x1a,
    2,
    0x12,
    0,
    0x60,
    1,
  ]);
  const validated = kernel.validateView(view);
  expect(validated.id).toHaveLength(64);
  expect(validated.references).toEqual([
    { kind: OP_REFERENCE_KIND.commit, id: new Uint8Array(20).fill(1) },
  ]);
});

it("rejects swapped and duplicate tree entries through Wasm", () => {
  const kernel = new Kernel();
  const first = treeEntry("100644", "a", 0x11);
  const second = treeEntry("100644", "b", 0x22);

  expect(() => kernel.validate(1, concatenate(second, first))).toThrow(
    "not in canonical Git order",
  );
  expect(() =>
    kernel.validate(1, concatenate(first, second, treeEntry("100755", "b", 0x33))),
  ).toThrow("duplicates an earlier name");
});

it("stays usable after a trap, so one poisoned request cannot wedge the object", () => {
  const kernel = new Kernel();
  const payload = treeEntry("100644", "a", 0x11);
  const expected = kernel.validate(1, payload);

  expect(() =>
    kernel.contained(() => {
      throw new WebAssembly.RuntimeError("unreachable");
    }),
  ).toThrow(WebAssembly.RuntimeError);

  expect(kernel.validate(1, payload)).toEqual(expected);
  expect(kernel.hash([payload])).toEqual(new Kernel().hash([payload]));
});

function decodeRle(value: string): Uint8Array {
  return Uint8Array.from(
    value.split(",").flatMap((run) => {
      const [byte, count] = run.split("*");
      return Array(Number.parseInt(count, 10)).fill(Number.parseInt(byte, 16));
    }),
  );
}

function treeEntry(mode: string, name: string, oidByte: number): Uint8Array {
  return concatenate(
    new TextEncoder().encode(`${mode} ${name}`),
    new Uint8Array([0]),
    new Uint8Array(20).fill(oidByte),
  );
}

function concatenate(...parts: Uint8Array[]): Uint8Array {
  const result = new Uint8Array(parts.reduce((length, part) => length + part.byteLength, 0));
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.byteLength;
  }
  return result;
}
