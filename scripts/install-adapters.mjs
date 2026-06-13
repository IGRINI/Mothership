// Installs the built provider adapters into the running app's plugins directory
// so they appear as connectors in Settings. Dev convenience until there is a
// real "store"/install flow — the app scans `<app-data>/plugins/<name>/adapter.json`.
//
// It builds each `adapters/*` crate and writes a manifest into the plugins dir
// whose `program` points at the freshly built binary (absolute path) and whose
// integrity matches that binary. Re-run this after rebuilding adapters so the
// installed manifest hash stays current. Refresh Settings to see them.
//
// Usage:
//   node scripts/install-adapters.mjs            # debug binaries
//   node scripts/install-adapters.mjs --release  # release binaries

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const APP_IDENTIFIER = "dev.mothership.desktop";
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const release = process.argv.includes("--release");
const profile = release ? "release" : "debug";
const targetDirectory = process.env.CARGO_TARGET_DIR
  ? resolve(repoRoot, process.env.CARGO_TARGET_DIR)
  : join(repoRoot, "target");
const explicitCargoTarget = process.env.CARGO_BUILD_TARGET?.trim();

function appDataDir() {
  if (process.platform === "win32") {
    const roaming = process.env.APPDATA ?? join(homedir(), "AppData", "Roaming");
    return join(roaming, APP_IDENTIFIER);
  }
  if (process.platform === "darwin") {
    return join(homedir(), "Library", "Application Support", APP_IDENTIFIER);
  }
  const dataHome = process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share");
  return join(dataHome, APP_IDENTIFIER);
}

const adaptersRoot = join(repoRoot, "adapters");
const manifestPaths = readdirSync(adaptersRoot, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => join(adaptersRoot, entry.name, "adapter.json"))
  .filter((path) => existsSync(path));

if (manifestPaths.length === 0) {
  console.error("no adapters/*/adapter.json found");
  process.exit(1);
}

const pluginsDir = join(appDataDir(), "plugins");
let installed = 0;

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

for (const manifestPath of manifestPaths) {
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const { provider_id, provider_label, program } = manifest;
  // The crate's binary name is the manifest program without the .exe suffix;
  // on this OS the built file may or may not carry .exe.
  const crate = program.replace(/\.exe$/i, "");
  const binaryName = process.platform === "win32" ? `${crate}.exe` : crate;

  console.log(`building ${crate} (${profile})...`);
  execFileSync("cargo", ["build", ...(release ? ["--release"] : []), "-p", crate], {
    cwd: repoRoot,
    stdio: "inherit",
  });

  const builtBinary = join(
    targetDirectory,
    ...(explicitCargoTarget ? [explicitCargoTarget] : []),
    profile,
    binaryName,
  );
  if (!existsSync(builtBinary)) {
    console.warn(`skip ${provider_id}: built binary not found at ${builtBinary}`);
    continue;
  }

  const installedManifest = {
    provider_id,
    provider_label,
    program: builtBinary,
    integrity: {
      algorithm: "sha256",
      sha256: sha256File(builtBinary),
    },
  };
  if (Array.isArray(manifest.capabilities)) {
    installedManifest.capabilities = manifest.capabilities;
  }

  const dest = join(pluginsDir, provider_id);
  mkdirSync(dest, { recursive: true });

  // The adapter ships its own icon next to its manifest; copy it into the
  // plugin folder and reference it by a relative name — the host only serves
  // icons that live inside the plugin folder.
  if (manifest.icon) {
    const sourceIcon = join(dirname(manifestPath), manifest.icon);
    if (existsSync(sourceIcon)) {
      const iconName = basename(manifest.icon);
      copyFileSync(sourceIcon, join(dest, iconName));
      installedManifest.icon = iconName;
    } else {
      console.warn(`skip icon for ${provider_id}: not found at ${sourceIcon}`);
    }
  }

  writeFileSync(
    join(dest, "adapter.json"),
    `${JSON.stringify(installedManifest, null, 2)}\n`,
  );
  console.log(`installed ${provider_id} -> ${join(dest, "adapter.json")}`);
  console.log(`         program: ${builtBinary}`);
  installed += 1;
}

console.log(`\n${installed} adapter(s) installed into ${pluginsDir}`);
console.log("Open Settings and hit Refresh to see the connectors.");
