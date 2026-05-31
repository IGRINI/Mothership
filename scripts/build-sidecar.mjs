import { copyFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";

const release = process.argv.includes("--release");
const profile = release ? "release" : "debug";
const executableExtension = process.platform === "win32" ? ".exe" : "";
const root = process.cwd();
const targetDirectory = process.env.CARGO_TARGET_DIR
  ? resolve(root, process.env.CARGO_TARGET_DIR)
  : join(root, "target");
const explicitCargoTarget = process.env.CARGO_BUILD_TARGET?.trim();

const targetTriple = execFileSync("rustc", ["--print", "host-tuple"], {
  encoding: "utf8",
}).trim();

if (!targetTriple) {
  throw new Error("rustc did not return a host target triple");
}

const sidecarBinary = "mothership-sidecar";
const adapterBinaries = ["codex-adapter", "openrouter-adapter"];
const bundledBinaries = [sidecarBinary, ...adapterBinaries];

for (const binary of adapterBinaries) {
  buildCargoBinary(binary);
}

const adapterHashes = new Map(
  adapterBinaries.map((binary) => [binary, sha256File(binaryPath(binary))]),
);

buildCargoBinary(sidecarBinary, {
  MOTHERSHIP_BUILTIN_CODEX_ADAPTER_SHA256: adapterHashes.get("codex-adapter"),
  MOTHERSHIP_BUILTIN_OPENROUTER_ADAPTER_SHA256: adapterHashes.get("openrouter-adapter"),
});

const destinationDirectory = join(root, "src-tauri", "binaries");
mkdirSync(destinationDirectory, { recursive: true });

for (const binary of bundledBinaries) {
  const source = binaryPath(binary);
  if (!existsSync(source)) {
    throw new Error(`expected bundled binary was not produced: ${source}`);
  }

  for (const target of sidecarTargetAliases(targetTriple)) {
    const destination = join(
      destinationDirectory,
      `${binary}-${target}${executableExtension}`,
    );
    copyFileSync(source, destination);
    console.log(`bundled binary ready: ${basename(destination)}`);
  }
}

function buildCargoBinary(binary, extraEnv = {}) {
  const cargoArgs = ["build", "-p", binary];
  if (release) {
    cargoArgs.push("--release");
  }
  execFileSync("cargo", cargoArgs, {
    cwd: root,
    env: { ...process.env, ...extraEnv },
    stdio: "inherit",
  });
}

function binaryPath(binary) {
  return join(
    targetDirectory,
    ...(explicitCargoTarget ? [explicitCargoTarget] : []),
    profile,
    `${binary}${executableExtension}`,
  );
}

function sha256File(path) {
  if (!existsSync(path)) {
    throw new Error(`expected adapter binary was not produced: ${path}`);
  }
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function sidecarTargetAliases(hostTriple) {
  const aliases = new Set([hostTriple]);

  if (process.platform === "win32") {
    const architecture = hostTriple.startsWith("aarch64") ? "aarch64" : "x86_64";
    aliases.add(`${architecture}-pc-windows-gnu`);
    aliases.add(`${architecture}-pc-windows-msvc`);
  }

  return aliases;
}
