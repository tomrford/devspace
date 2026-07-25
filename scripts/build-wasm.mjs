import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, statSync } from "node:fs";

// The names src/kernel.ts casts the instance to. A Rust rename would otherwise
// only surface as a runtime TypeError inside a Durable Object.
const KERNEL_EXPORTS = [
  "memory",
  "kernel_alloc",
  "kernel_dealloc",
  "kernel_validate",
  "kernel_validate_view",
  "kernel_validate_operation",
  "kernel_hash_new",
  "kernel_hash_update",
  "kernel_hash_finish",
  "kernel_hash_drop",
];

buildWasm({
  packageName: "devspace-kernel-wasm",
  artifactName: "devspace_kernel_wasm.wasm",
  outputName: "kernel.wasm",
  budget: 200 * 1024,
  requiredExports: KERNEL_EXPORTS,
});

function buildWasm({ packageName, artifactName, outputName, budget, requiredExports }) {
  run("cargo", [
    "build",
    "--profile",
    "wasm-release",
    "--target",
    "wasm32-unknown-unknown",
    "-p",
    packageName,
  ]);

  mkdirSync("dist", { recursive: true });
  const source = `target/wasm32-unknown-unknown/wasm-release/${artifactName}`;
  const output = `dist/${outputName}`;
  run("wasm-opt", [
    "-Oz",
    "--enable-bulk-memory",
    "--enable-bulk-memory-opt",
    source,
    "-o",
    output,
  ]);

  const wasmBytes = statSync(output).size;
  const module = new WebAssembly.Module(readFileSync(output));
  const imports = WebAssembly.Module.imports(module);
  if (imports.length !== 0) {
    throw new Error(`${output} has ${imports.length} WebAssembly imports`);
  }
  const exported = new Set(WebAssembly.Module.exports(module).map((entry) => entry.name));
  const missing = requiredExports.filter((name) => !exported.has(name));
  if (missing.length !== 0) {
    throw new Error(`${output} is missing the exports ${missing.join(", ")}`);
  }
  console.log(
    `${output}: ${wasmBytes} bytes, zero imports, ${requiredExports.length} required exports`,
  );
  if (budget !== undefined && wasmBytes > budget) {
    throw new Error(`optimized validation kernel is ${wasmBytes} bytes; budget is ${budget}`);
  }
}

function run(command, arguments_) {
  const result = spawnSync(command, arguments_, { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
