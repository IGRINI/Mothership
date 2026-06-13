// Recognizing file references in model output: markdown links/images whose
// target is a workspace or absolute local path (`src/foo.ts:42`, `file://…`,
// `E:\repo\bar.rs`), plain inline paths in prose, and the line-number suffixes
// both may carry. Pure string parsing — the markdown components decide how to
// render and open what this module recognizes.

export interface MarkdownFileLinkTarget {
  path: string;
  line?: number;
  copyPath: string;
  contentType?: string;
}

export interface PlainFileReferenceSegment {
  text: string;
  target?: MarkdownFileLinkTarget;
}

export function parseMarkdownFileLink(
  href: string | undefined,
  label: string,
): MarkdownFileLinkTarget | undefined {
  const raw = href?.trim();
  if (!raw) {
    return undefined;
  }

  const parsed = parseFileReference(raw);
  if (!parsed || !isLikelyLocalFilePath(parsed.path)) {
    return undefined;
  }

  const line = parsed.line ?? parseLineNumber(label);
  const contentType = imageContentTypeForPath(parsed.path);
  return {
    path: parsed.path,
    line,
    copyPath: line ? `${parsed.path}:${line}` : parsed.path,
    contentType,
  };
}

const PLAIN_LOCAL_FILE_PATTERN =
  /(?:file:\/\/\/[^\s<>"'`]+|[a-zA-Z]:[\\/][^\s<>"'`]+)/g;

/** Split prose into plain-text segments and recognized local-file references
 * (absolute paths / file:// URLs appearing outside markdown link syntax). */
export function splitPlainLocalFileReferences(
  text: string,
): PlainFileReferenceSegment[] {
  if (!text) {
    return [{ text }];
  }

  const segments: PlainFileReferenceSegment[] = [];
  let lastIndex = 0;
  for (const match of text.matchAll(PLAIN_LOCAL_FILE_PATTERN)) {
    const raw = match[0];
    const index = match.index ?? 0;
    if (index > lastIndex) {
      segments.push({ text: text.slice(lastIndex, index) });
    }

    const parsed = parsePlainLocalFileReference(raw);
    if (parsed) {
      segments.push({ text: parsed.label, target: parsed.target });
      if (parsed.trailing) {
        segments.push({ text: parsed.trailing });
      }
    } else {
      segments.push({ text: raw });
    }
    lastIndex = index + raw.length;
  }

  if (lastIndex < text.length) {
    segments.push({ text: text.slice(lastIndex) });
  }
  return segments.length > 0 ? segments : [{ text }];
}

function parsePlainLocalFileReference(raw: string) {
  let candidate = raw;
  let trailing = "";
  while (candidate.length > 0) {
    const target = parseMarkdownFileLink(candidate, candidate);
    if (target) {
      return { label: candidate, target, trailing };
    }
    const last = candidate[candidate.length - 1];
    if (!last || !/[),.;!?]/.test(last)) {
      return undefined;
    }
    trailing = last + trailing;
    candidate = candidate.slice(0, -1);
  }
  return undefined;
}

function parseFileReference(raw: string): { path: string; line?: number } | undefined {
  let value = decodeLinkText(stripWrappingAngleBrackets(raw.trim()));
  if (!value || value.startsWith("#") || hasExternalScheme(value)) {
    return undefined;
  }

  if (value.toLowerCase().startsWith("file://")) {
    return parseFileUrlReference(value);
  }

  let line: number | undefined;
  const hash = splitSuffix(value, "#");
  if (hash) {
    value = hash.before;
    line = parseLineNumber(hash.after);
  }
  const query = splitSuffix(value, "?");
  if (query) {
    value = query.before;
    line ??= parseLineNumber(query.after);
  }

  const stripped = stripInlineLineSuffix(value);
  return normalizeParsedPath(stripped.path, stripped.line ?? line);
}

function parseFileUrlReference(raw: string) {
  try {
    const url = new URL(raw);
    let path = decodeLinkText(url.pathname);
    if (/^\/[a-zA-Z]:\//.test(path)) {
      path = path.slice(1);
    }
    if (url.hostname) {
      path = `//${url.hostname}${path}`;
    }
    const line = parseLineNumber(url.hash) ?? parseLineNumber(url.search);
    const stripped = stripInlineLineSuffix(path);
    return normalizeParsedPath(stripped.path, stripped.line ?? line);
  } catch {
    return undefined;
  }
}

function normalizeParsedPath(path: string, line?: number) {
  const normalized = path.trim().replace(/\\/g, "/");
  if (!normalized) {
    return undefined;
  }
  return { path: normalized, line };
}

function stripInlineLineSuffix(path: string): { path: string; line?: number } {
  let value = path.trim();
  const annotation = value.match(/\s+\((?:line|строка)\s+(\d+)\)\s*$/i);
  if (annotation) {
    value = value.slice(0, annotation.index).trimEnd();
    return { path: value, line: Number(annotation[1]) };
  }

  const match = value.match(/^(.*):(\d+)(?::\d+)?$/);
  if (!match) {
    return { path: value };
  }

  const pathPart = match[1];
  if (!pathPart || /^[a-zA-Z]$/.test(pathPart)) {
    return { path: value };
  }
  if (!isLikelyLocalFilePath(pathPart)) {
    return { path: value };
  }
  return { path: pathPart, line: Number(match[2]) };
}

function splitSuffix(value: string, marker: "#" | "?") {
  const index = value.indexOf(marker);
  return index >= 0
    ? { before: value.slice(0, index), after: value.slice(index + 1) }
    : undefined;
}

function parseLineNumber(value: string | undefined): number | undefined {
  if (!value) {
    return undefined;
  }
  const match = value.match(
    /(?:^|[^a-z0-9])(?:line|строка|l|#)\s*[=#:\-\s]*\s*(\d+)/i,
  );
  if (!match) {
    return undefined;
  }
  const line = Number(match[1]);
  return Number.isSafeInteger(line) && line > 0 ? line : undefined;
}

function hasExternalScheme(value: string) {
  const scheme = value.match(/^([a-z][a-z0-9+.-]*):/i)?.[1]?.toLowerCase();
  return Boolean(
    scheme && !/^[a-z]$/i.test(scheme) && !scheme.includes(".") && scheme !== "file",
  );
}

function isLikelyLocalFilePath(path: string) {
  const value = path.trim();
  if (!value) {
    return false;
  }
  if (isAbsoluteLocalFilePath(value)) {
    return true;
  }
  if (value.includes("/") || value.includes("\\")) {
    return true;
  }
  return hasKnownFileExtension(value);
}

export function isAbsoluteLocalFilePath(path: string) {
  return /^[a-zA-Z]:[\\/]/.test(path) || /^[/\\]/.test(path);
}

/** Whether opening this path must go through the artifact (absolute-path)
 * commands instead of the workspace-scoped ones. */
export function usesArtifactActions(projectId: string | undefined, path: string) {
  const value = path.trim();
  return (
    isMothershipArtifactPath(value) ||
    (!projectId && isAbsoluteLocalFilePath(value))
  );
}

function isMothershipArtifactPath(path: string) {
  const normalized = path.replace(/\\/g, "/").toLowerCase();
  return (
    isAbsoluteLocalFilePath(path) &&
    normalized.includes("/artifacts/") &&
    (normalized.includes("/appdata/") ||
      normalized.includes("/library/application support/") ||
      normalized.includes("/.local/share/"))
  );
}

function hasKnownFileExtension(path: string) {
  const name = path.split(/[\\/]/).pop() ?? path;
  const extension = name.match(/\.([a-z0-9][a-z0-9_-]{0,12})$/i)?.[1];
  if (!extension) {
    return false;
  }
  return KNOWN_MARKDOWN_FILE_EXTENSIONS.has(extension.toLowerCase());
}

function imageContentTypeForPath(path: string): string | undefined {
  const extension = fileExtension(path);
  if (!extension) {
    return undefined;
  }
  return IMAGE_FILE_CONTENT_TYPES[extension];
}

function fileExtension(path: string) {
  return path
    .split(/[\\/]/)
    .pop()
    ?.match(/\.([a-z0-9][a-z0-9_-]{0,12})$/i)?.[1]
    ?.toLowerCase();
}

export function fileNameFromPath(path: string) {
  const normalized = path.replace(/\\/g, "/");
  const name = normalized.split("/").filter(Boolean).pop();
  return name && name.trim().length > 0 ? name : path;
}

const IMAGE_FILE_CONTENT_TYPES: Record<string, string> = {
  apng: "image/apng",
  avif: "image/avif",
  bmp: "image/bmp",
  gif: "image/gif",
  ico: "image/x-icon",
  jfif: "image/jpeg",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
  png: "image/png",
  svg: "image/svg+xml",
  webp: "image/webp",
};

const KNOWN_MARKDOWN_FILE_EXTENSIONS = new Set([
  ...Object.keys(IMAGE_FILE_CONTENT_TYPES),
  "c",
  "cpp",
  "cs",
  "css",
  "csv",
  "go",
  "html",
  "java",
  "js",
  "json",
  "jsx",
  "kt",
  "lock",
  "log",
  "md",
  "mdx",
  "php",
  "py",
  "rs",
  "sql",
  "toml",
  "ts",
  "tsx",
  "txt",
  "xml",
  "yaml",
  "yml",
]);

function stripWrappingAngleBrackets(value: string) {
  return value.startsWith("<") && value.endsWith(">")
    ? value.slice(1, -1).trim()
    : value;
}

function decodeLinkText(value: string) {
  try {
    return decodeURI(value);
  } catch {
    return value;
  }
}
