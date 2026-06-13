import { For, Show, createSignal } from "solid-js";
import { ChevronRight, File as FileIcon, Folder } from "lucide-solid";

import { FileActions, onFileContextMenu } from "../../../shared/ui/FileActions";

interface TreeNode {
  name: string;
  /** Full path from the listing root (used for open/reveal). */
  path: string;
  children: Map<string, TreeNode>;
}

/** Build a directory tree from a flat list of "/"-separated paths. */
function buildTree(paths: string[]): TreeNode {
  const root: TreeNode = { name: "", path: "", children: new Map() };
  for (const raw of paths) {
    const clean = raw.replace(/\\/g, "/").replace(/\/+$/, "").trim();
    if (!clean) {
      continue;
    }
    let node = root;
    let acc = "";
    for (const part of clean.split("/").filter(Boolean)) {
      acc = acc ? `${acc}/${part}` : part;
      let child = node.children.get(part);
      if (!child) {
        child = { name: part, path: acc, children: new Map() };
        node.children.set(part, child);
      }
      node = child;
    }
  }
  return root;
}

// Directories first, then files, each alphabetical.
function sortNodes(a: TreeNode, b: TreeNode): number {
  const aDir = a.children.size > 0;
  const bDir = b.children.size > 0;
  if (aDir !== bDir) {
    return aDir ? -1 : 1;
  }
  return a.name.localeCompare(b.name);
}

function TreeRow(props: { node: TreeNode; projectId?: string; depth: number }) {
  const isDir = () => props.node.children.size > 0;
  const [open, setOpen] = createSignal(true);
  const children = () => [...props.node.children.values()].sort(sortNodes);
  const indent = () => `${4 + props.depth * 14}px`;

  return (
    <>
      <Show
        when={isDir()}
        fallback={
          <div
            class="file-tree__row file-tree__row--file"
            style={{ "padding-left": indent() }}
            onContextMenu={(event) =>
              onFileContextMenu(event, {
                projectId: props.projectId,
                path: props.node.path,
              })
            }
          >
            <span class="file-tree__spacer" />
            <FileIcon class="file-tree__icon" size={13} />
            <span class="file-tree__name">{props.node.name}</span>
            <FileActions
              class="file-tree__actions"
              projectId={props.projectId}
              path={props.node.path}
            />
          </div>
        }
      >
        <button
          class="file-tree__row file-tree__row--dir"
          type="button"
          style={{ "padding-left": indent() }}
          onClick={() => setOpen(!open())}
        >
          <ChevronRight
            classList={{
              "file-tree__chevron": true,
              "file-tree__chevron--open": open(),
            }}
            size={13}
          />
          <Folder class="file-tree__icon" size={13} />
          <span class="file-tree__name">{props.node.name}</span>
          <span class="file-tree__count">{props.node.children.size}</span>
        </button>
      </Show>
      <Show when={isDir() && open()}>
        <For each={children()}>
          {(child) => (
            <TreeRow
              node={child}
              projectId={props.projectId}
              depth={props.depth + 1}
            />
          )}
        </For>
      </Show>
    </>
  );
}

/** Expandable file tree for command output that is already normalized to paths. */
export function FileTree(props: { paths: string[]; projectId?: string }) {
  const roots = () => [...buildTree(props.paths).children.values()].sort(sortNodes);
  return (
    <div class="file-tree">
      <For each={roots()}>
        {(node) => (
          <TreeRow node={node} projectId={props.projectId} depth={0} />
        )}
      </For>
    </div>
  );
}
