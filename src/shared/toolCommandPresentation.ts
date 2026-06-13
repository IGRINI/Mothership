import type { ToolCommand, ToolExecutionResult } from "./api/mothership";

export type CommandIntent =
  | "generic"
  | "list"
  | "read"
  | "search"
  | "git_status"
  | "git_diff"
  | "git_show"
  | "git_log"
  | "test"
  | "build"
  | "install"
  | "check"
  | "location";

export type CommandOutputView =
  | "terminal"
  | "file_tree"
  | "text"
  | "diff"
  | "search"
  | "status";

export interface CommandPresentationInput {
  command?: ToolCommand | null;
  payload?: Record<string, unknown> | null;
  result?: ToolExecutionResult | null;
  output?: string | null;
}

export interface CommandPresentation {
  intent: CommandIntent;
  outputView: CommandOutputView;
  commandLine: string;
  headline: string;
  activityLabel: string;
  target?: string;
  stdout: string;
  stderr: string;
  exitCode?: number;
  truncated: boolean;
  logRef?: string;
  paths: string[];
}

const COMMAND_INTENTS: ReadonlySet<string> = new Set<CommandIntent>([
  "generic",
  "list",
  "read",
  "search",
  "git_status",
  "git_diff",
  "git_show",
  "git_log",
  "test",
  "build",
  "install",
  "check",
  "location",
]);

export function commandPresentation(
  input: CommandPresentationInput,
): CommandPresentation {
  const command = commandFromInput(input);
  const intent = payloadIntent(input.payload) ?? inferCommandIntent(command);
  const commandLine = formatCommandLine(command);
  const target = commandTarget(command, intent);
  const stdout = firstText(
    readString(input.payload, "stdoutPreview"),
    input.result?.stdoutPreview,
    input.result?.stdoutTail,
    input.output ?? undefined,
  );
  const stderr = firstText(
    readString(input.payload, "stderrPreview"),
    input.result?.stderrPreview,
    input.result?.stderrTail,
  );
  const paths = intent === "list" ? listingPathsFromOutput(stdout) : [];
  const outputView = commandOutputView(intent, stdout, paths);

  return {
    intent,
    outputView,
    commandLine,
    headline: commandHeadline(intent, commandLine, target),
    activityLabel: commandActivityLabel(intent),
    target,
    stdout,
    stderr,
    exitCode: readNumber(input.payload, "exitCode") ?? input.result?.exitCode ?? undefined,
    truncated:
      readBoolean(input.payload, "truncated") ??
      input.result?.truncatedForDisplay ??
      false,
    logRef: readString(input.payload, "logRef") ?? input.result?.logRef ?? undefined,
    paths,
  };
}

export function listingPathsFromOutput(output: string): string[] {
  const seen = new Set<string>();
  const paths: string[] = [];
  for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
    const path = listingPathFromLine(rawLine);
    if (!path || seen.has(path)) {
      continue;
    }
    seen.add(path);
    paths.push(path);
    if (paths.length >= 500) {
      break;
    }
  }
  return paths;
}

export function looksLikeUnifiedDiff(text: string): boolean {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  return (
    lines.some((line) => line.startsWith("diff --git ")) ||
    (lines.some((line) => line.startsWith("@@ ")) &&
      lines.some((line) => line.startsWith("+++ ") || line.startsWith("--- ")))
  );
}

function commandOutputView(
  intent: CommandIntent,
  stdout: string,
  paths: string[],
): CommandOutputView {
  if (intent === "list" && paths.length > 0) {
    return "file_tree";
  }
  if (intent === "read") {
    return "text";
  }
  if (intent === "search") {
    return "search";
  }
  if (intent === "git_status") {
    return "status";
  }
  if (
    intent === "git_diff" ||
    (intent === "git_show" && looksLikeUnifiedDiff(stdout))
  ) {
    return "diff";
  }
  return "terminal";
}

function commandHeadline(
  intent: CommandIntent,
  commandLine: string,
  target: string | undefined,
): string {
  switch (intent) {
    case "list":
      return target ? `Listed ${target}` : "Listed files";
    case "read":
      return target ? `Read ${target}` : "Read content";
    case "search":
      return target ? `Searched ${target}` : "Searched";
    case "git_status":
      return "Checked git status";
    case "git_diff":
      return target ? `Viewed diff for ${target}` : "Viewed git diff";
    case "git_show":
      return target ? `Viewed ${target}` : "Viewed git show";
    case "git_log":
      return "Viewed git log";
    case "test":
      return "Ran tests";
    case "build":
      return "Built project";
    case "install":
      return "Installed dependencies";
    case "check":
      return "Checked project";
    case "location":
      return "Checked location";
    case "generic":
      return commandLine || "Ran command";
  }
}

function commandActivityLabel(intent: CommandIntent): string {
  switch (intent) {
    case "list":
      return "Listing files";
    case "read":
      return "Reading output";
    case "search":
      return "Searching code";
    case "git_status":
      return "Checking git status";
    case "git_diff":
    case "git_show":
    case "git_log":
      return "Inspecting git";
    case "test":
      return "Running tests";
    case "build":
      return "Building";
    case "install":
      return "Installing";
    case "check":
      return "Checking";
    case "location":
      return "Checking location";
    case "generic":
      return "Running command";
  }
}

function commandFromInput(input: CommandPresentationInput): ToolCommand {
  const program =
    input.command?.program ?? readString(input.payload, "program") ?? "";
  const args =
    input.command?.args ?? readStringArray(input.payload, "args") ?? [];
  return {
    program,
    args,
    env: input.command?.env ?? {},
  };
}

function formatCommandLine(command: ToolCommand): string {
  return [command.program, ...command.args.map(quoteArg)]
    .filter((part) => part.trim().length > 0)
    .join(" ")
    .trim();
}

function quoteArg(arg: string): string {
  if (!arg || /[\s"'`]/.test(arg)) {
    return JSON.stringify(arg);
  }
  return arg;
}

function payloadIntent(
  payload: Record<string, unknown> | null | undefined,
): CommandIntent | undefined {
  const value = readString(payload, "commandIntent");
  return value && COMMAND_INTENTS.has(value) ? (value as CommandIntent) : undefined;
}

function inferCommandIntent(command: ToolCommand): CommandIntent {
  const program = programName(command.program);
  switch (program) {
    case "git":
      return gitIntent(command.args);
    case "ls":
    case "dir":
      return "list";
    case "cat":
    case "type":
      return "read";
    case "grep":
    case "findstr":
      return "search";
    case "pwd":
      return "location";
    case "npm":
    case "pnpm":
    case "yarn":
    case "bun":
      return packageManagerIntent(command.args);
    case "cargo":
      return cargoIntent(command.args);
    case "powershell":
    case "pwsh":
      return shellCommandIntent(powershellCommandText(command.args));
    case "cmd":
      return cmdCommandIntent(command.args);
    case "bash":
    case "sh":
      return shellScriptArgIntent(command.args);
    default:
      return "generic";
  }
}

function gitIntent(args: string[]): CommandIntent {
  switch ((args[0] ?? "").toLowerCase()) {
    case "status":
      return "git_status";
    case "diff":
      return "git_diff";
    case "show":
      return "git_show";
    case "log":
      return "git_log";
    case "ls-files":
      return "list";
    default:
      return "generic";
  }
}

function packageManagerIntent(args: string[]): CommandIntent {
  const words = args.map((arg) => arg.toLowerCase());
  if (words.some((arg) => arg === "test" || arg === "tests")) {
    return "test";
  }
  if (words.includes("build")) {
    return "build";
  }
  if (words.some((arg) => arg === "install" || arg === "add")) {
    return "install";
  }
  return "generic";
}

function cargoIntent(args: string[]): CommandIntent {
  switch ((args[0] ?? "").toLowerCase()) {
    case "test":
      return "test";
    case "build":
      return "build";
    case "check":
      return "check";
    default:
      return "generic";
  }
}

function shellCommandIntent(command: string): CommandIntent {
  const tokens = shellWords(command);
  const first = programName(tokens[0] ?? "");
  switch (first) {
    case "get-childitem":
    case "gci":
    case "ls":
    case "dir":
      return "list";
    case "get-content":
    case "gc":
    case "cat":
    case "type":
      return "read";
    case "select-string":
    case "sls":
    case "grep":
    case "findstr":
      return "search";
    case "get-location":
    case "pwd":
      return "location";
    case "git":
      return gitIntent(tokens.slice(1));
    case "npm":
    case "pnpm":
    case "yarn":
    case "bun":
      return packageManagerIntent(tokens.slice(1));
    case "cargo":
      return cargoIntent(tokens.slice(1));
    default:
      return "generic";
  }
}

function powershellCommandText(args: string[]): string {
  const index = args.findIndex(
    (arg) => arg.toLowerCase() === "-command" || arg.toLowerCase() === "-c",
  );
  return index >= 0 ? args.slice(index + 1).join(" ") : (args[0] ?? "");
}

function cmdCommandIntent(args: string[]): CommandIntent {
  const index = args.findIndex(
    (arg) => arg.toLowerCase() === "/c" || arg.toLowerCase() === "-c",
  );
  return shellCommandIntent(
    index >= 0 ? args.slice(index + 1).join(" ") : args.join(" "),
  );
}

function shellScriptArgIntent(args: string[]): CommandIntent {
  const index = args.findIndex((arg) =>
    ["-c", "-lc", "--command"].includes(arg.toLowerCase()),
  );
  return shellCommandIntent(
    index >= 0 ? args.slice(index + 1).join(" ") : args.join(" "),
  );
}

function commandTarget(
  command: ToolCommand,
  intent: CommandIntent,
): string | undefined {
  const program = programName(command.program);
  if (program === "powershell" || program === "pwsh") {
    return targetFromTokens(shellWords(powershellCommandText(command.args)), intent);
  }
  if (program === "cmd") {
    const index = command.args.findIndex(
      (arg) => arg.toLowerCase() === "/c" || arg.toLowerCase() === "-c",
    );
    const text =
      index >= 0 ? command.args.slice(index + 1).join(" ") : command.args.join(" ");
    return targetFromTokens(shellWords(text), intent);
  }
  if (program === "bash" || program === "sh") {
    const index = command.args.findIndex((arg) =>
      ["-c", "-lc", "--command"].includes(arg.toLowerCase()),
    );
    const text =
      index >= 0 ? command.args.slice(index + 1).join(" ") : command.args.join(" ");
    return targetFromTokens(shellWords(text), intent);
  }
  return targetFromTokens([program, ...command.args], intent);
}

function targetFromTokens(
  tokens: string[],
  intent: CommandIntent,
): string | undefined {
  if (tokens.length === 0) {
    return undefined;
  }
  const [program, ...args] = tokens;
  if (programName(program) === "git") {
    const candidates = args.slice(1).filter((arg) => !arg.startsWith("-"));
    return candidates[0];
  }
  if (intent === "search") {
    return args.filter((arg) => !arg.startsWith("-"))[1];
  }
  if (intent === "list" || intent === "read") {
    return args.filter((arg) => !arg.startsWith("-"))[0];
  }
  return undefined;
}

function shellWords(command: string): string[] {
  const words: string[] = [];
  let current = "";
  let quote: string | undefined;
  for (const ch of command) {
    if (quote) {
      if (ch === quote) {
        quote = undefined;
      } else {
        current += ch;
      }
      continue;
    }
    if (ch === "'" || ch === '"') {
      quote = ch;
      continue;
    }
    if (/\s/.test(ch)) {
      if (current) {
        words.push(current);
        current = "";
      }
      continue;
    }
    current += ch;
  }
  if (current) {
    words.push(current);
  }
  return words;
}

function programName(program: string): string {
  let name =
    program
      .split(/[\\/]/)
      .filter(Boolean)
      .pop()
      ?.trim()
      .toLowerCase() ?? "";
  for (const suffix of [".exe", ".cmd", ".bat", ".ps1"]) {
    if (name.endsWith(suffix)) {
      name = name.slice(0, -suffix.length);
      break;
    }
  }
  return name;
}

function listingPathFromLine(rawLine: string): string | undefined {
  const line = rawLine.trim();
  if (!line || isListingHeader(line)) {
    return undefined;
  }

  const longLs = /^[bcdlps-][rwxstST-]{9}\s+\S+\s+\S+\s+\S+\s+\S+\s+\S+\s+\S+\s+\S+\s+(.+)$/.exec(
    line,
  );
  if (longLs?.[1]) {
    return cleanListingName(longLs[1]);
  }

  const columns = line.split(/\s{2,}/).filter(Boolean);
  if (columns.length >= 3 && /^[dlarhs-]{3,}/i.test(columns[0])) {
    return cleanListingName(columns[columns.length - 1]);
  }

  if (looksLikeSinglePath(line)) {
    return cleanListingName(line);
  }

  return undefined;
}

function cleanListingName(value: string): string | undefined {
  const name = value.replace(/\s+->\s+.+$/, "").trim();
  if (!name || name === "." || name === "..") {
    return undefined;
  }
  return name.replace(/\\/g, "/");
}

function isListingHeader(line: string): boolean {
  return (
    /^total\s+\d+/i.test(line) ||
    /^directory:\s+/i.test(line) ||
    /^mode\s+lastwritetime/i.test(line.replace(/\s+/g, " ")) ||
    /^-+\s+-+/i.test(line) ||
    /^\d+\s+file\(s\)/i.test(line) ||
    /^\d+\s+dir\(s\)/i.test(line)
  );
}

function looksLikeSinglePath(line: string): boolean {
  if (line.length > 240 || /^\$|^>|^\[/.test(line)) {
    return false;
  }
  if (line.includes("\t")) {
    return false;
  }
  return (
    /^[A-Za-z]:[\\/]/.test(line) ||
    /^[./~]?[A-Za-z0-9_.@+-][A-Za-z0-9_.@+\-\\/ ]*$/.test(line)
  );
}

function firstText(...values: Array<string | null | undefined>): string {
  for (const value of values) {
    if (typeof value === "string" && value.trim().length > 0) {
      return value.trim();
    }
  }
  return "";
}

function readString(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): string | undefined {
  const value = payload?.[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function readNumber(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): number | undefined {
  const value = payload?.[key];
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function readBoolean(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): boolean | undefined {
  const value = payload?.[key];
  return typeof value === "boolean" ? value : undefined;
}

function readStringArray(
  payload: Record<string, unknown> | null | undefined,
  key: string,
): string[] | undefined {
  const value = payload?.[key];
  if (!Array.isArray(value)) {
    return undefined;
  }
  const strings = value.filter(
    (item): item is string => typeof item === "string",
  );
  return strings.length > 0 ? strings : undefined;
}
