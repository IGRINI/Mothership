import { ExternalLink, FolderOpen } from "lucide-solid";

import { openToolPath, revealToolPath } from "../api/mothership";
import { openContextMenu, type ContextMenuItem } from "./ContextMenu";

/** Open a workspace file in the OS default app. Errors surface via `onError`. */
export function openWorkspaceFile(
  projectId: string | undefined,
  path: string,
  onError?: (message: string) => void,
) {
  if (!path) {
    return;
  }
  openToolPath(projectId, path).catch((error: unknown) =>
    onError?.(errorText(error, "не удалось открыть файл")),
  );
}

/** Reveal a workspace file in the OS file manager (Explorer / Finder). */
export function revealWorkspaceFile(
  projectId: string | undefined,
  path: string,
  onError?: (message: string) => void,
) {
  if (!path) {
    return;
  }
  revealToolPath(projectId, path).catch((error: unknown) =>
    onError?.(errorText(error, "не удалось показать файл в папке")),
  );
}

function errorText(error: unknown, fallback: string): string {
  if (typeof error === "string") {
    return error;
  }
  return error instanceof Error ? error.message : fallback;
}

interface FileActionTarget {
  projectId?: string;
  path: string;
  // When set, "Открыть" calls this instead of opening directly — lets a host
  // surface open errors in its own UI. Reveal always goes through the API.
  onOpen?: (path: string) => void;
  onError?: (message: string) => void;
}

function fileMenuItems(target: FileActionTarget): ContextMenuItem[] {
  return [
    {
      label: "Открыть",
      onSelect: () =>
        target.onOpen
          ? target.onOpen(target.path)
          : openWorkspaceFile(target.projectId, target.path, target.onError),
    },
    {
      label: "Показать в папке",
      onSelect: () =>
        revealWorkspaceFile(target.projectId, target.path, target.onError),
    },
    {
      label: "Копировать путь",
      onSelect: () => {
        navigator.clipboard?.writeText(target.path).catch(() => {});
      },
    },
  ];
}

/** Right-click handler that pops the custom file context menu at the cursor. */
export function onFileContextMenu(event: MouseEvent, target: FileActionTarget) {
  if (!target.path) {
    return;
  }
  event.preventDefault();
  event.stopPropagation();
  openContextMenu({
    x: event.clientX,
    y: event.clientY,
    items: fileMenuItems(target),
  });
}

/** Inline "Открыть" / "В папке" link-buttons for a workspace file. */
export function FileActions(props: {
  projectId?: string;
  path: string;
  onOpen?: (path: string) => void;
  onError?: (message: string) => void;
  class?: string;
}) {
  return (
    <span class={`file-actions ${props.class ?? ""}`}>
      <button
        class="file-actions__btn"
        type="button"
        title="Открыть во внешнем приложении"
        onClick={(event) => {
          event.stopPropagation();
          if (props.onOpen) {
            props.onOpen(props.path);
          } else {
            openWorkspaceFile(props.projectId, props.path, props.onError);
          }
        }}
      >
        <ExternalLink size={12} />
        Открыть
      </button>
      <button
        class="file-actions__btn"
        type="button"
        title="Показать в папке"
        onClick={(event) => {
          event.stopPropagation();
          revealWorkspaceFile(props.projectId, props.path, props.onError);
        }}
      >
        <FolderOpen size={12} />
        В папке
      </button>
    </span>
  );
}
