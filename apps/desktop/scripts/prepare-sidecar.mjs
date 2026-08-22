import { execFileSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  mkdirSync,
  renameSync,
  rmSync,
  statSync,
} from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const desktopDirectory = resolve(scriptDirectory, "..");
const workspaceDirectory = resolve(desktopDirectory, "..", "..");
const cargo = process.env.CARGO?.trim() || "cargo";

function commandOutput(command, args) {
  return execFileSync(command, args, {
    cwd: workspaceDirectory,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  }).trim();
}

function targetTriple() {
  const fromTauri = process.env.TAURI_ENV_TARGET_TRIPLE?.trim();
  const triple = fromTauri || commandOutput("rustc", ["--print", "host-tuple"]);
  if (!/^[A-Za-z0-9_.-]+$/.test(triple)) {
    throw new Error(`Invalid Rust target triple: ${JSON.stringify(triple)}`);
  }
  if (triple === "universal-apple-darwin") {
    throw new Error(
      "Universal macOS sidecars require a universal cmr binary; build one architecture at a time.",
    );
  }
  return triple;
}

const triple = targetTriple();
const debugBuild = /^(1|true)$/i.test(process.env.TAURI_ENV_DEBUG?.trim() || "");
const profile = debugBuild ? "debug" : "release";
const extension = triple.includes("windows") ? ".exe" : "";
const sidecarTargetDirectory = resolve(workspaceDirectory, "target", "sidecar");
const cargoArguments = [
  "build",
  "--locked",
  "--package",
  "cmr-cli",
  "--bin",
  "cmr",
  "--bin",
  "cmr-service",
  "--target",
  triple,
  "--target-dir",
  sidecarTargetDirectory,
];
if (!debugBuild) {
  cargoArguments.push("--release");
}

execFileSync(cargo, cargoArguments, {
  cwd: workspaceDirectory,
  stdio: "inherit",
});

const binariesDirectory = resolve(desktopDirectory, "src-tauri", "binaries");
mkdirSync(binariesDirectory, { recursive: true });

for (const binary of ["cmr", "cmr-service"]) {
  const compiled = resolve(
    sidecarTargetDirectory,
    triple,
    profile,
    `${binary}${extension}`,
  );
  const bundled = resolve(
    binariesDirectory,
    `${binary}-${triple}${extension}`,
  );
  const temporary = `${bundled}.${process.pid}.tmp`;

  if (!statSync(compiled).isFile() || statSync(compiled).size === 0) {
    throw new Error(
      `Cargo did not produce a usable ${binary} executable at ${compiled}`,
    );
  }

  rmSync(temporary, { force: true });
  copyFileSync(compiled, temporary);
  if (!statSync(temporary).isFile() || statSync(temporary).size === 0) {
    throw new Error(`Failed to stage the ${binary} sidecar at ${temporary}`);
  }
  rmSync(bundled, { force: true });
  renameSync(temporary, bundled);
  if (!triple.includes("windows")) {
    chmodSync(bundled, 0o755);
  }

  console.log(`Prepared Tauri sidecar: ${bundled}`);
}
