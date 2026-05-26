import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";

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

const cargoArgs = ["build", "-p", "mothership-sidecar"];
if (release) {
  cargoArgs.push("--release");
}

execFileSync("cargo", cargoArgs, { cwd: root, stdio: "inherit" });

const source = join(
  targetDirectory,
  ...(explicitCargoTarget ? [explicitCargoTarget] : []),
  profile,
  `mothership-sidecar${executableExtension}`,
);
const destinationDirectory = join(root, "src-tauri", "binaries");
if (!existsSync(source)) {
  throw new Error(`expected sidecar binary was not produced: ${source}`);
}

mkdirSync(destinationDirectory, { recursive: true });

for (const target of sidecarTargetAliases(targetTriple)) {
  const destination = join(
    destinationDirectory,
    `mothership-sidecar-${target}${executableExtension}`,
  );
  copyFileSync(source, destination);
  console.log(`sidecar ready: ${basename(destination)}`);
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
