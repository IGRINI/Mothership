// Theme token generator for src/App.css.
//
// IT OWNS ONLY THE GENERATED HEADER (the `--c-RRGGBB` token blocks + palette
// blocks at the top of the file). Everything from the first base rule
// (`:root { color: ... }`) onward — i.e. ALL hand-authored CSS — is the BODY and
// is preserved BYTE-FOR-BYTE. The script reads App.css itself (NOT a frozen
// backup), so it can never clobber edits you made to the body. Re-running is
// idempotent.
//
// What it does each run:
//   1. Split the file into [old generated header] + [body] at the base rule.
//   2. In the BODY only: tokenize any raw hex you added (`#1b2a3a` →
//      `var(--c-1b2a3a)`), route white/structural overlays + the #ffffff special
//      cases. Already-tokenized body has none of these, so it's a no-op.
//   3. Collect every `--c-RRGGBB` the body references and (re)generate the
//      header: each token's dark value = its own hex, light value = HSL-light
//      inverse, plus per-palette re-tints.
//   4. Write [new header] + [unchanged body].
//
// So: add a rule, add a raw hex, restyle anything — just re-run and your light
// theme + palettes update automatically while your CSS is left intact.
//
// Run:  node scripts/gen-theme.mjs                 (rewrites src/App.css)
//       node scripts/gen-theme.mjs in.css out.css  (test on a copy)

import { readFileSync, writeFileSync } from "node:fs";

const IN_PATH = process.argv[2] ?? "src/App.css";
const OUT_PATH = process.argv[3] ?? IN_PATH;

const raw = readFileSync(IN_PATH, "utf8");

// ---- color math -----------------------------------------------------------
function hexToRgb(h) {
  const n = parseInt(h, 16);
  return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
}
function rgbToHex(r, g, b) {
  const c = (v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0");
  return `#${c(r)}${c(g)}${c(b)}`;
}
function rgbToHsl(r, g, b) {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  let h = 0, s = 0;
  const l = (max + min) / 2;
  const d = max - min;
  if (d !== 0) {
    s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    switch (max) {
      case r: h = (g - b) / d + (g < b ? 6 : 0); break;
      case g: h = (b - r) / d + 2; break;
      default: h = (r - g) / d + 4; break;
    }
    h /= 6;
  }
  return { h, s, l };
}
function hslToRgb(h, s, l) {
  if (s === 0) { const v = l * 255; return { r: v, g: v, b: v }; }
  const hue = (p, q, t) => {
    if (t < 0) t += 1;
    if (t > 1) t -= 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  return { r: hue(p, q, h + 1 / 3) * 255, g: hue(p, q, h) * 255, b: hue(p, q, h - 1 / 3) * 255 };
}

// Dark hex -> light hex: invert lightness (compressed away from pure black/
// white), and damp saturation as colors get lighter so pale surfaces read as
// clean blue-grey rather than candy tints. Mid-lightness accents/status colors
// barely move, so they stay vivid in both themes.
function toLight(hex) {
  const { r, g, b } = hexToRgb(hex);
  const { h, s, l } = rgbToHsl(r, g, b);
  let nl = 0.045 + (1 - l) * 0.91;
  let ns = s;
  if (nl > 0.82) ns = s * 0.5;
  else if (nl > 0.66) ns = s * 0.78;
  nl = Math.min(0.985, Math.max(0.03, nl));
  const { r: R, g: G, b: B } = hslToRgb(h, ns, nl);
  return rgbToHex(R, G, B);
}

// ---- split header / body --------------------------------------------------
// The body begins at the first base color rule; the header above it is ours to
// regenerate. If we can't find it we REFUSE to write rather than risk the body.
const marker = raw.search(/:root\s*\{\s*\r?\n\s*color:/);
if (marker === -1) {
  throw new Error(
    "gen-theme: could not find the base ':root { color: ... }' rule that starts " +
      "the body. Refusing to write so no hand-authored CSS is lost.",
  );
}
let body = raw.slice(marker);
const bodyBefore = body;

// ---- routing rules (BODY ONLY) --------------------------------------------
// Hexes aliased to the existing semantic tokens (keep accent/structure live).
const ALIAS = {
  "#050a10": "--bg-root",
  "#07101a": "--bg-shell",
  "#216ce3": "--accent",
  "#7fa6d8": "--accent-soft",
};
// Handled by hand (ambiguous or theme-independent), never auto-tokenized.
const SKIP = new Set(["#000", "#ffffff"]);

// Tokenize any raw hex the author wrote (idempotent — tokenized CSS has none).
let tokenized = 0;
body = body.replace(/#([0-9a-fA-F]{6}|[0-9a-fA-F]{3})\b/g, (m, hex) => {
  if (hex !== hex.toLowerCase()) return m;
  const key = `#${hex.toLowerCase()}`;
  if (SKIP.has(key)) return m;
  if (ALIAS[key]) return `var(${ALIAS[key]})`;
  if (hex.length === 3) return m; // only #000 is 3-digit and it's skipped
  tokenized += 1;
  return `var(--c-${hex.toLowerCase()})`;
});

// White highlight overlays must flip to dark overlays in light mode.
body = body.replace(/rgb\(\s*255\s+255\s+255\s*\/\s*([\d.]+%)\s*\)/g, "rgb(var(--fg-rgb) / $1)");
body = body.replace(/rgba\(\s*255\s*,\s*255\s*,\s*255\s*,\s*([\d.]+)\s*\)/g, "rgb(var(--fg-rgb) / calc($1 * 100%))");

// #ffffff disambiguation: white on a translucent fg overlay flips dark in light;
// white on a SOLID colored fill (accent/red/avatar) stays white via --on-accent.
body = body.replace(
  /(background:\s*rgb\(var\(--fg-rgb\)\s*\/\s*\d+%\);\s*\r?\n\s*)color:\s*#ffffff/g,
  "$1color: var(--text-strong)",
);
body = body.replace(/\b(color|background):\s*#ffffff\b/g, "$1: var(--on-accent)");
// Body text inside the accent-colored user bubble stays light in both themes.
body = body.replace(/var\(--c-f4f8fd\)/g, "var(--on-accent-dim)");

// Structural surface fades/scrims authored as colored rgba flip with the theme.
body = body.replace(/rgb\(\s*7\s+16\s+26\s*\//g, "rgb(var(--bg-shell-rgb) /");
body = body.replace(/rgb\(\s*4\s+13\s+24\s*\//g, "rgb(var(--bg-root-rgb) /");

// ---- collect the tokens the body actually references ----------------------
const used = new Map(); // "#rrggbb" -> light hex
for (const mm of body.matchAll(/var\(--c-([0-9a-f]{6})\)/g)) {
  const key = `#${mm[1]}`;
  if (!used.has(key)) used.set(key, toLight(mm[1]));
}
const keys = [...used.keys()].sort();
const darkLines = keys.map((k) => `  --c-${k.slice(1)}: ${k};`).join("\n");
const lightLines = keys.map((k) => `  --c-${k.slice(1)}: ${used.get(k)};`).join("\n");

// ---- palettes -------------------------------------------------------------
// A palette is a *flavor* applied on top of the light/dark mode: it re-tints
// the whole NEUTRAL family (surfaces, borders, blue-grey text) by rotating hue
// and scaling saturation/lightness, while leaving status colors (green/red/
// amber) and the accent untouched. aurora == the base palette (no block).
const PALETTES = [
  // id, hueShift°, satScale, darkLightnessScale (only deepens dark surfaces)
  { id: "midnight", dh: 8, ks: 0.6, kld: 0.68 }, // cooler, desaturated, darker
  { id: "graphite", dh: 0, ks: 0.12, kld: 0.95 }, // neutral slate
  { id: "nebula", dh: 52, ks: 1.04, kld: 1.0 }, // violet tint
];

const clamp01 = (v) => Math.max(0, Math.min(1, v));

// A token is "neutral" (palette-tintable) if it's near-grey or sits in the
// blue/violet band — i.e. a surface/border/text tone, not a status color.
function isNeutral(hex) {
  const { r, g, b } = hexToRgb(hex.replace("#", ""));
  const { h, s } = rgbToHsl(r, g, b);
  const hue = h * 360;
  return s < 0.08 || (hue >= 180 && hue <= 262);
}

// Re-tint one color for a palette. kl deepens only already-dark tones so light
// text stays bright when a palette darkens its surfaces.
function tint(hex, dh, ks, kl) {
  const { r, g, b } = hexToRgb(hex.replace("#", ""));
  let { h, s, l } = rgbToHsl(r, g, b);
  h = ((h + dh / 360) % 1 + 1) % 1;
  s = clamp01(s * ks);
  if (l < 0.5) l = clamp01(l * kl);
  const c = hslToRgb(h, s, l);
  return rgbToHex(c.r, c.g, c.b);
}

const channels = (hex) => {
  const { r, g, b } = hexToRgb(hex.replace("#", ""));
  return `${r} ${g} ${b}`;
};

const neutralKeys = keys.filter((k) => isNeutral(k));

function paletteBlock(p) {
  const surfaces = (mode, srcRoot, srcShell, kl) => {
    const root = tint(srcRoot, p.dh, p.ks, kl);
    const shell = tint(srcShell, p.dh, p.ks, kl);
    const lines = neutralKeys
      .map((k) => {
        const src = mode === "dark" ? k : used.get(k);
        return `  --c-${k.slice(1)}: ${tint(src, p.dh, p.ks, kl)};`;
      })
      .join("\n");
    return (
      `  --bg-root: ${root};\n  --bg-shell: ${shell};\n` +
      `  --bg-root-rgb: ${channels(root)};\n  --bg-shell-rgb: ${channels(shell)};\n` +
      lines
    );
  };
  return (
    `:root[data-palette="${p.id}"] {\n` +
    surfaces("dark", "#050a10", "#07101a", p.kld) +
    `\n}\n\n` +
    `:root[data-palette="${p.id}"][data-theme="light"] {\n` +
    surfaces("light", "#e8edf3", "#f6f8fb", 1) +
    `\n}\n`
  );
}

const paletteBlocks = PALETTES.map(paletteBlock).join("\n");

// Hand-tuned light overrides: a clean light hierarchy (grey page, near-white
// panels, soft insets) on top of the generated inverses, plus readable accent
// links. These win because they come after the generated tokens in the block.
const LIGHT_TUNE = `
  /* hand-tuned light surfaces — cleaner card/page hierarchy than raw inverse */
  --c-0c1722: #ffffff; --c-0b1622: #ffffff; --c-0d1722: #ffffff;
  --c-101b27: #f4f7fa; --c-0f1a26: #f4f7fa; --c-101d2b: #f4f7fa;
  --c-16222f: #eef2f7; --c-1b2a3a: #e7edf4; --c-1d2b3b: #e7edf4;
  --c-223244: #e1e8f0; --c-182838: #eef2f7; --c-142335: #f4f7fa;
  --c-0a141f: #f7f9fc; --c-091420: #f7f9fc; --c-070f18: #f7f9fc;
  /* keep accent-tinted hairlines/links legible on light */
  --c-7fa6d8: #2f6fd8; --c-a9c6ef: #2f6fd8;`;

const NEW_HEADER = `/* ============================================================================
   GENERATED appearance tokens — produced by scripts/gen-theme.mjs.

   DO NOT hand-edit anything between here and the base ':root { color: ... }'
   rule below: re-running the generator overwrites this whole block. Everything
   from that base rule onward is hand-authored and is preserved verbatim, so add
   your own rules freely (raw hex you use gets tokenized + a light variant on the
   next run). To re-tune the light palette or flavors, edit gen-theme.mjs.

   Theme model: Mode (System / Light / Dark) × Palette (data-palette). The base
   :root is the DARK Aurora palette; [data-theme=light] flips it; each
   [data-palette=...] block re-tints the neutral family in either mode.
   ============================================================================ */
:root {
  color-scheme: dark;

  --bg-root: #050a10;
  --bg-shell: #07101a;
  --bg-root-rgb: 5 10 16;
  --bg-shell-rgb: 7 16 26;

  --accent: #216ce3;
  --accent-soft: #7fa6d8;
  --accent-contrast: #ffffff;

  --text-strong: #ffffff;
  --on-accent: #ffffff;
  --on-accent-dim: #eef4fd;
  --fg-rgb: 255 255 255;

  --app-font-sans:
    Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont,
    "Segoe UI", sans-serif;
  --app-font-mono: ui-monospace, "Cascadia Code", Consolas, "Courier New", monospace;
  --app-ui-scale: 1;

  /* generated dark color tokens */
${darkLines}
}

:root[data-accent="iris"]    { --accent: #7c5cff; --accent-soft: #b6a8ff; }
:root[data-accent="emerald"] { --accent: #18a558; --accent-soft: #5fcf90; }
:root[data-accent="amber"]   { --accent: #e0a32e; --accent-soft: #e8c15a; }
:root[data-accent="rose"]    { --accent: #e25577; --accent-soft: #ff9bb0; }
:root[data-accent="cyan"]    { --accent: #1fa8c9; --accent-soft: #76d6e8; }

:root[data-theme="light"] {
  color-scheme: light;

  --bg-root: #e8edf3;
  --bg-shell: #f6f8fb;
  --bg-root-rgb: 232 237 243;
  --bg-shell-rgb: 246 248 251;

  --accent-contrast: #ffffff;
  --text-strong: #0b1622;
  --on-accent: #ffffff;
  --on-accent-dim: #eef4fd;
  --fg-rgb: 18 28 40;

  /* generated light color tokens (HSL-lightness inverse of the dark palette) */
${lightLines}
${LIGHT_TUNE}
}

/* ----------------------------------------------------------------------------
   Palette flavors (data-palette). Each re-tints the neutral surface/text family
   over the active light/dark mode; aurora is the un-attributed base. Generated.
   ---------------------------------------------------------------------------- */
${paletteBlocks}`;

writeFileSync(OUT_PATH, NEW_HEADER + body);

// ---- report ---------------------------------------------------------------
console.log(`in:  ${IN_PATH}`);
console.log(`out: ${OUT_PATH}`);
console.log(`tokens defined: ${keys.length}  (neutral/palette-tinted: ${neutralKeys.length})`);
console.log(`raw hex tokenized in body this run: ${tokenized}`);
console.log(`body preserved: ${body === bodyBefore ? "byte-for-byte (no raw colors)" : `${tokenized} colors tokenized, otherwise intact`}`);
