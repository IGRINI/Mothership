import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
let env = { ...process.env };

function normalizeWindowsPath(value) {
  return value.replaceAll("/", "\\");
}

function prependPath(envValue, pathToPrepend) {
  const key = Object.keys(envValue).find((name) => name.toLowerCase() === "path") ?? "Path";
  const current = envValue[key] ?? "";
  const normalized = normalizeWindowsPath(pathToPrepend);
  const parts = current
    .split(";")
    .map((part) => part.trim())
    .filter(Boolean)
    .filter((part) => normalizeWindowsPath(part) !== normalized);

  envValue[key] = [normalized, ...parts].join(";");
}

function hasOption(name) {
  return args.some((arg) => arg === name || arg.startsWith(`${name}=`));
}

function hasRustToolchain(name) {
  const result = spawnSync("rustup", ["toolchain", "list"], {
    encoding: "utf8",
    env,
    windowsHide: true,
  });

  return result.status === 0 && result.stdout.includes(name);
}

function configureWindowsRust() {
  if (env.USERPROFILE) {
    prependPath(env, path.join(env.USERPROFILE, ".cargo", "bin"));
  }

  const preferredToolchain = "stable-x86_64-pc-windows-msvc";
  const hasExplicitToolchain = Boolean(env.MOTHERSHIP_RUSTUP_TOOLCHAIN || env.RUSTUP_TOOLCHAIN);

  if (!hasExplicitToolchain && hasRustToolchain(preferredToolchain)) {
    env.RUSTUP_TOOLCHAIN = preferredToolchain;
  } else if (env.MOTHERSHIP_RUSTUP_TOOLCHAIN) {
    env.RUSTUP_TOOLCHAIN = env.MOTHERSHIP_RUSTUP_TOOLCHAIN;
  }

  const mode = args[0] ?? "tauri";
  const targetSuffix =
    mode === "dev" ? "tauri-dev" : mode === "build" ? "tauri-build" : "tauri";

  env.CARGO_TARGET_DIR ||= path.join(process.cwd(), `target-${targetSuffix}`);

  if (mode === "dev" && !hasOption("--no-watch")) {
    args.splice(1, 0, "--no-watch");
  }
}

if (process.platform === "win32") {
  configureWindowsRust();
}

const localNodeBin = path.join(process.cwd(), "node_modules", ".bin");
const localTauriCommand = path.join(localNodeBin, process.platform === "win32" ? "tauri.cmd" : "tauri");

const command = process.platform === "win32" ? process.env.ComSpec || "cmd.exe" : localTauriCommand;
const commandArgs =
  process.platform === "win32"
    ? ["/d", "/s", "/c", existsSync(localTauriCommand) ? localTauriCommand : "tauri.cmd", ...args]
    : args;

const child = spawn(command, commandArgs, {
  env,
  stdio: "inherit",
});

child.on("exit", (code) => {
  process.exit(code ?? 1);
});
