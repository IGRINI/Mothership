import { Match, Switch } from "solid-js";
import { FileText, Folder, GitBranch, Search, Terminal } from "lucide-solid";

import type { ToolKind } from "../../../shared/api/mothership";

export function ToolKindIcon(props: { kind?: ToolKind }) {
  return (
    <Switch fallback={<Terminal size={14} />}>
      <Match when={props.kind === "list_files"}>
        <Folder size={14} />
      </Match>
      <Match when={props.kind === "search_text"}>
        <Search size={14} />
      </Match>
      <Match when={props.kind === "apply_patch"}>
        <GitBranch size={14} />
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
