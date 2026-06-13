// Regenerates the TypeScript wire types that mirror the Rust serde structs, via
// ts-rs. The `#[derive(TS)] #[ts(export)]` types across `mothership-core` and
// `mothership-adapter-protocol` each write a `<Type>.ts` into
// `src/shared/api/generated/` when their generated export test runs under
// `cargo test`; this script drives that and then rebuilds the `index.ts` barrel
// that re-exports them all.
//
// The export directory + integer mapping are configured in `.cargo/config.toml`
// ([env] TS_RS_EXPORT_DIR / TS_RS_LARGE_INT), so they apply to every crate's
// export test uniformly — this script only invokes the tests and stitches the
// barrel.
//
// Usage:
//   node scripts/gen-types.mjs           # regenerate generated/*.ts + index.ts
//   node scripts/gen-types.mjs --check   # regenerate, then fail if it drifted
//                                          from what's committed (CI drift guard)
//
// `--check` is the CI contract: it regenerates and runs `git diff --exit-code`
// over the generated dir. A non-empty diff means a Rust wire type changed but the
// committed TS wasn't regenerated — the build fails so the drift can't merge.

import { execFileSync } from "node:child_process";
import { readdirSync, writeFileSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const generatedDir = join(repoRoot, "src", "shared", "api", "generated");
const barrelPath = join(generatedDir, "index.ts");
const check = process.argv.includes("--check");

// The crates whose `#[ts(export)]` types feed `generated/`. Running each crate's
// export tests (filtered to the ts-rs-generated `export_bindings_*` tests) writes
// the .ts files. Kept explicit so adding a new wire-type crate is a one-line edit.
const CRATES = ["mothership-core", "mothership-adapter-protocol"];

function run(cmd, args) {
  console.log(`$ ${cmd} ${args.join(" ")}`);
  execFileSync(cmd, args, { cwd: repoRoot, stdio: "inherit" });
}

function regenerate() {
  for (const crate of CRATES) {
    // `export_bindings` filters to the ts-rs-generated export tests, so this
    // doesn't run (or depend on) the rest of the crate's test suite.
    run("cargo", ["test", "-p", crate, "export_bindings"]);
  }
  writeBarrel();
}

// Build `index.ts` re-exporting every generated type, so call sites can import
// the whole wire surface from one module. Deterministic (sorted) so the barrel
// never spuriously drifts. Skips `index.ts` itself and the nested `serde_json/`
// helper dir (an internal JsonValue impl, not part of the public surface).
function writeBarrel() {
  const files = readdirSync(generatedDir, { withFileTypes: true })
    .filter(
      (entry) =>
        entry.isFile() &&
        entry.name.endsWith(".ts") &&
        entry.name !== "index.ts",
    )
    .map((entry) => entry.name.slice(0, -3))
    .sort((a, b) => a.localeCompare(b, "en"));

  const header =
    "// AUTO-GENERATED barrel for the ts-rs wire types. Do not edit by hand —\n" +
    "// run `npm run types:gen` (see scripts/gen-types.mjs). Re-exports every\n" +
    "// generated type so the frontend imports the wire surface from one module.\n\n";
  const body = files.map((name) => `export type * from "./${name}";`).join("\n");
  writeFileSync(barrelPath, `${header}${body}\n`, "utf8");
  console.log(
    `Wrote ${barrelPath} re-exporting ${files.length} generated types.`,
  );
}

function assertNoDrift() {
  let diff = "";
  try {
    // --exit-code: zero status + empty output means the regenerated files match
    // what's committed. Untracked new files won't show here, so also check status.
    diff = execFileSync(
      "git",
      ["diff", "--", "src/shared/api/generated"],
      { cwd: repoRoot, encoding: "utf8" },
    );
  } catch (error) {
    // git itself failed (not a drift) — surface it.
    console.error("git diff failed:", error.message);
    process.exit(2);
  }
  const status = execFileSync(
    "git",
    ["status", "--porcelain", "--", "src/shared/api/generated"],
    { cwd: repoRoot, encoding: "utf8" },
  ).trim();

  if (diff.trim() || status) {
    console.error(
      "\nGenerated wire types are out of date. A Rust wire type changed but the\n" +
        "committed TypeScript wasn't regenerated. Run `npm run types:gen` and commit\n" +
        "src/shared/api/generated/.\n",
    );
    if (diff.trim()) console.error(diff);
    if (status) console.error(status);
    process.exit(1);
  }
  console.log("Generated wire types are up to date.");
}

regenerate();
if (check) {
  assertNoDrift();
}
