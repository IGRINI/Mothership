import { Match, Switch } from "solid-js";
import {
  FileText,
  Folder,
  GitBranch,
  Image,
  Search,
  Terminal,
} from "lucide-solid";

import type { ToolKind } from "../../../shared/api/mothership";
import type { CommandIntent } from "../../../shared/toolCommandPresentation";

export function ToolKindIcon(props: {
  kind?: ToolKind;
  commandIntent?: CommandIntent;
}) {
  return (
    <Switch fallback={<Terminal size={14} />}>
      <Match
        when={
          props.kind === "run_command" && props.commandIntent === "list"
        }
      >
        <Folder size={14} />
      </Match>
      <Match
        when={props.kind === "run_command" && props.commandIntent === "read"}
      >
        <FileText size={14} />
      </Match>
      <Match
        when={props.kind === "run_command" && props.commandIntent === "search"}
      >
        <Search size={14} />
      </Match>
      <Match
        when={
          props.kind === "run_command" &&
          props.commandIntent?.startsWith("git_")
        }
      >
        <GitBranch size={14} />
      </Match>
      <Match when={props.kind === "search_text"}>
        <Search size={14} />
      </Match>
      <Match when={props.kind === "apply_patch"}>
        <GitBranch size={14} />
      </Match>
      <Match when={props.kind === "image_generate"}>
        <Image size={14} />
      </Match>
      <Match
        when={
          props.kind === "read_file" ||
          props.kind === "write_file" ||
          props.kind === "edit_file"
        }
      >
        <FileText size={14} />
      </Match>
    </Switch>
  );
}
