// Tiny, dependency-free syntax highlighter.
//
// Deliberately NOT a real parser: a single regex splits a line into comment /
// string / keyword / number / flag tokens. It is language-agnostic on purpose —
// the tool spoiler renders snippets from Go, Rust, TS, Lua, shell, … and a
// best-effort coloring reads better than none without dragging in a highlighting
// engine. Tokens are returned as data (not HTML) so the caller renders them as
// SolidJS text nodes — no `innerHTML`, no escaping, no XSS surface.

export interface HighlightToken {
  /** CSS class for the token, or "" for plain text rendered verbatim. */
  cls: string;
  text: string;
}

// One pass, four capture groups: (1) line comment, (2) quoted string,
// (3) keyword, (4) number. The keyword set is the union of the languages we
// actually show; an unknown identifier simply stays plain.
const CODE_PATTERN =
  /(\/\/[^\n]*|#[^\n]*|<!--[\s\S]*?-->|--[^\n]*)|("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)|\b(func|fn|return|if|else|elif|for|while|range|loop|match|switch|case|break|continue|package|import|from|use|mod|pub|const|let|var|type|struct|interface|enum|trait|impl|where|class|def|new|go|defer|async|await|move|self|Self|crate|super|null|nil|None|Some|Ok|Err|true|false|undefined|export|function|then|end|local|require)\b|\b\d[\w.]*\b/g;

/** Tokenize one line of code for display. Never throws; falls back to plain. */
export function highlightCode(line: string): HighlightToken[] {
  const tokens: HighlightToken[] = [];
  let last = 0;
  let match: RegExpExecArray | null;
  CODE_PATTERN.lastIndex = 0;
  while ((match = CODE_PATTERN.exec(line))) {
    if (match.index > last) {
      tokens.push({ cls: "", text: line.slice(last, match.index) });
    }
    if (match[1]) {
      tokens.push({ cls: "tok-com", text: match[1] });
    } else if (match[2]) {
      tokens.push({ cls: "tok-str", text: match[2] });
    } else if (match[3]) {
      tokens.push({ cls: "tok-kw", text: match[3] });
    } else {
      tokens.push({ cls: "tok-num", text: match[0] });
    }
    last = match.index + match[0].length;
  }
  if (last < line.length) {
    tokens.push({ cls: "", text: line.slice(last) });
  }
  return tokens;
}

// Highlights flags in a shell command: `-x`, `--flag`, `/S` (Windows-style).
const FLAG_PATTERN = /(^|\s)((?:--?|\/)[A-Za-z][\w-]*)/g;

/** Tokenize a command line, coloring flags. Never throws. */
export function highlightCommand(text: string): HighlightToken[] {
  const tokens: HighlightToken[] = [];
  let last = 0;
  let match: RegExpExecArray | null;
  FLAG_PATTERN.lastIndex = 0;
  while ((match = FLAG_PATTERN.exec(text))) {
    const flagStart = match.index + match[1].length;
    if (flagStart > last) {
      tokens.push({ cls: "", text: text.slice(last, flagStart) });
    }
    tokens.push({ cls: "tok-flag", text: match[2] });
    last = flagStart + match[2].length;
  }
  if (last < text.length) {
    tokens.push({ cls: "", text: text.slice(last) });
  }
  return tokens;
}
