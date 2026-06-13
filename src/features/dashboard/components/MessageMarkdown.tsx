// Markdown rendering for chat messages, with workspace-aware affordances: file
// references (links, inline code, plain paths in prose) become clickable
// openers with context menus, and image references render as inline previews
// that open the lightbox. Rendering uses SolidMarkdown's "reconcile" strategy
// so streamed deltas patch text nodes instead of re-mounting the tree.

import { createEffect, createSignal, For, onCleanup, Show, type JSX } from "solid-js";
import { convertFileSrc } from "@tauri-apps/api/core";
import { SolidMarkdown, type SolidMarkdownComponents } from "solid-markdown";
import remarkGfm from "remark-gfm";

import {
  openArtifactPath,
  openExternalUrl,
  readImageDataUrlWithTimeout,
} from "../../../shared/api/mothership";
import { onFileContextMenu, openWorkspaceFile } from "../../../shared/ui/FileActions";
import {
  fileNameFromPath,
  isAbsoluteLocalFilePath,
  parseMarkdownFileLink,
  splitPlainLocalFileReferences,
  usesArtifactActions,
  type MarkdownFileLinkTarget,
} from "../file-references";
import { errorMessage } from "../message-model";
import type { ToolImagePreviewItem } from "../ToolCards";

type MarkdownAnchorProps = JSX.AnchorHTMLAttributes<HTMLAnchorElement> & {
  node?: unknown;
};

type MarkdownImageProps = JSX.ImgHTMLAttributes<HTMLImageElement> & {
  node?: unknown;
};

type MarkdownCodeProps = JSX.IntrinsicElements["code"] & {
  inline?: boolean;
  node?: unknown;
};

export function MessageMarkdown(props: {
  content: string;
  projectId?: string;
  onError?: (message: string) => void;
  onPreviewImage?: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
}) {
  const components: SolidMarkdownComponents = {
    a: (linkProps) => (
      <MarkdownLink
        {...linkProps}
        projectId={props.projectId}
        onOpenError={props.onError}
        onPreviewImage={props.onPreviewImage}
      />
    ),
    img: (imageProps) => (
      <MarkdownImage
        {...imageProps}
        projectId={props.projectId}
        onOpenError={props.onError}
        onPreviewImage={props.onPreviewImage}
      />
    ),
    code: (codeProps) => (
      <MarkdownCode
        {...codeProps}
        projectId={props.projectId}
        onOpenError={props.onError}
        onPreviewImage={props.onPreviewImage}
      />
    ),
    text: (textProps) => (
      <MarkdownText
        node={textProps.node}
        projectId={props.projectId}
        onError={props.onError}
        onPreviewImage={props.onPreviewImage}
      />
    ),
  };

  return (
    <div class="message-md">
      <SolidMarkdown
        components={components}
        renderingStrategy="reconcile"
        remarkPlugins={[remarkGfm]}
        children={props.content}
      />
    </div>
  );
}

function openMarkdownFileTarget(
  projectId: string | undefined,
  target: MarkdownFileLinkTarget,
  onError?: (message: string) => void,
) {
  if (usesArtifactActions(projectId, target.path)) {
    openArtifactPath(target.path).catch((error: unknown) =>
      onError?.(errorMessage(error)),
    );
    return;
  }
  openWorkspaceFile(projectId, target.path, onError);
}

function MarkdownPlainFileReference(props: {
  target: MarkdownFileLinkTarget;
  label?: string;
  code?: boolean;
  projectId?: string;
  onError?: (message: string) => void;
  onPreviewImage?: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
}) {
  const label = () => props.label || props.target.copyPath;
  const openFile = (event: MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    openMarkdownFileTarget(props.projectId, props.target, props.onError);
  };
  const openMenu = (event: MouseEvent) => {
    onFileContextMenu(event, {
      projectId: props.projectId,
      path: props.target.path,
      copyPath: props.target.copyPath,
      artifact: usesArtifactActions(props.projectId, props.target.path),
      onError: props.onError,
    });
  };

  return (
    <Show
      when={props.target.contentType}
      fallback={
        <a
          class={props.code ? "message-file-code-link" : "message-file-link"}
          href={props.target.copyPath}
          title={props.target.copyPath}
          onClick={openFile}
          onContextMenu={openMenu}
        >
          <Show when={props.code} fallback={label()}>
            <code>{label()}</code>
          </Show>
        </a>
      }
    >
      <MarkdownImagePreview
        target={props.target}
        label={fileNameFromPath(props.target.path)}
        projectId={props.projectId}
        onError={props.onError}
        onPreviewImage={props.onPreviewImage}
      />
    </Show>
  );
}

function MarkdownText(props: {
  node: unknown;
  projectId?: string;
  onError?: (message: string) => void;
  onPreviewImage?: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
}) {
  const segments = () => splitPlainLocalFileReferences(extractHastText(props.node));
  return (
    <>
      <For each={segments()}>
        {(segment) => (
          <Show
            when={segment.target}
            fallback={segment.text}
          >
            {(target) => (
              <MarkdownPlainFileReference
                target={target()}
                label={segment.text}
                projectId={props.projectId}
                onError={props.onError}
                onPreviewImage={props.onPreviewImage}
              />
            )}
          </Show>
        )}
      </For>
    </>
  );
}

function MarkdownCode(
  props: MarkdownCodeProps & {
    projectId?: string;
    onOpenError?: (message: string) => void;
    onPreviewImage?: (
      image: ToolImagePreviewItem,
      images: ToolImagePreviewItem[],
    ) => void;
  },
) {
  const text = () => childrenText(props.children);
  const target = () =>
    props.inline ? parseMarkdownFileLink(text(), text()) : undefined;

  return (
    <Show
      when={target()}
      fallback={<code class={props.class}>{props.children}</code>}
    >
      {(value) => (
        <MarkdownPlainFileReference
          target={value()}
          label={text()}
          code
          projectId={props.projectId}
          onError={props.onOpenError}
          onPreviewImage={props.onPreviewImage}
        />
      )}
    </Show>
  );
}

function MarkdownLink(
  props: MarkdownAnchorProps & {
    projectId?: string;
    onOpenError?: (message: string) => void;
    onPreviewImage?: (
      image: ToolImagePreviewItem,
      images: ToolImagePreviewItem[],
    ) => void;
  },
) {
  const labelText = () => extractHastText(props.node);
  const fileTarget = () => parseMarkdownFileLink(props.href, labelText());
  const imageTarget = () => {
    const target = fileTarget();
    return target?.contentType ? target : undefined;
  };

  const openFile = (event: MouseEvent) => {
    const target = fileTarget();
    if (!target) {
      // Not a workspace file reference: never let the WebView navigate away.
      event.preventDefault();
      event.stopPropagation();
      const href = props.href ?? "";
      if (/^(https?|mailto|tel):/i.test(href)) {
        openExternalUrl(href).catch((error: unknown) =>
          props.onOpenError?.(errorMessage(error)),
        );
      }
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    openMarkdownFileTarget(props.projectId, target, props.onOpenError);
  };

  const openMenu = (event: MouseEvent) => {
    const target = fileTarget();
    if (!target) {
      return;
    }
    onFileContextMenu(event, {
      projectId: props.projectId,
      path: target.path,
      copyPath: target.copyPath,
      artifact: usesArtifactActions(props.projectId, target.path),
      onError: props.onOpenError,
    });
  };

  return (
    <Show
      when={imageTarget()}
      fallback={
        <a
          class={joinClass(
            props.class,
            fileTarget() ? "message-file-link" : undefined,
          )}
          href={props.href}
          rel={props.rel}
          target={props.target}
          title={props.title}
          onClick={openFile}
          onContextMenu={openMenu}
        >
          {props.children}
        </a>
      }
    >
      {(target) => (
        <MarkdownImagePreview
          target={target()}
          label={labelText() || fileNameFromPath(target().path)}
          projectId={props.projectId}
          onError={props.onOpenError}
          onPreviewImage={props.onPreviewImage}
        />
      )}
    </Show>
  );
}

function MarkdownImage(
  props: MarkdownImageProps & {
    projectId?: string;
    onOpenError?: (message: string) => void;
    onPreviewImage?: (
      image: ToolImagePreviewItem,
      images: ToolImagePreviewItem[],
    ) => void;
  },
) {
  const target = () => parseMarkdownFileLink(props.src, props.alt ?? "");
  const imageTarget = () => {
    const parsed = target();
    return parsed?.contentType ? parsed : undefined;
  };

  return (
    <Show
      when={imageTarget()}
      fallback={
        <img
          src={props.src}
          alt={props.alt}
          title={props.title}
          class={props.class}
          loading={props.loading}
        />
      }
    >
      {(target) => (
        <MarkdownImagePreview
          target={target()}
          label={props.alt || fileNameFromPath(target().path)}
          projectId={props.projectId}
          onError={props.onOpenError}
          onPreviewImage={props.onPreviewImage}
        />
      )}
    </Show>
  );
}

function MarkdownImagePreview(props: {
  target: MarkdownFileLinkTarget;
  label: string;
  projectId?: string;
  onError?: (message: string) => void;
  onPreviewImage?: (
    image: ToolImagePreviewItem,
    images: ToolImagePreviewItem[],
  ) => void;
}) {
  const [previewSrc, setPreviewSrc] = createSignal<string>();
  const [previewError, setPreviewError] = createSignal("");
  let resolveRequestId = 0;
  let triedDataUrl = false;

  createEffect(() => {
    const path = props.target.path;
    const projectId = props.projectId;
    const requestId = ++resolveRequestId;
    triedDataUrl = false;
    const initialSrc = isAbsoluteLocalFilePath(path) ? convertFileSrc(path) : "";
    setPreviewSrc(initialSrc || undefined);
    setPreviewError("");

    if (!initialSrc) {
      loadMarkdownImageDataUrl(requestId, projectId, path);
    }
  });

  onCleanup(() => {
    resolveRequestId += 1;
  });

  function loadMarkdownImageDataUrl(
    requestId = ++resolveRequestId,
    projectId = props.projectId,
    path = props.target.path,
  ) {
    if (triedDataUrl) {
      setPreviewSrc(undefined);
      setPreviewError("Preview unavailable");
      return;
    }
    triedDataUrl = true;
    setPreviewSrc(undefined);
    setPreviewError("");
    readImageDataUrlWithTimeout(projectId, path)
      .then((dataUrl) => {
        if (requestId !== resolveRequestId) {
          return;
        }
        setPreviewSrc(dataUrl);
      })
      .catch((error: unknown) => {
        if (requestId !== resolveRequestId) {
          return;
        }
        setPreviewError(errorMessage(error));
      });
  }

  const imageItem = (): ToolImagePreviewItem | undefined => {
    const src = previewSrc();
    if (!src) {
      return undefined;
    }
    return {
      id: `markdown-image:${props.target.path}`,
      src,
      path: props.target.path,
      label: props.label || fileNameFromPath(props.target.path),
      contentType: props.target.contentType ?? "image/*",
      sizeBytes: 0,
      preview: props.target.copyPath,
    };
  };

  const openPreview = (event: MouseEvent | KeyboardEvent) => {
    event.preventDefault();
    event.stopPropagation();
    const item = imageItem();
    if (item && props.onPreviewImage) {
      props.onPreviewImage(item, [item]);
      return;
    }
    openMarkdownFileTarget(props.projectId, props.target, props.onError);
  };

  const openMenu = (event: MouseEvent) => {
    onFileContextMenu(event, {
      projectId: props.projectId,
      path: props.target.path,
      copyPath: props.target.copyPath,
      artifact: usesArtifactActions(props.projectId, props.target.path),
      onError: props.onError,
    });
  };

  const handleKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Enter" || event.key === " ") {
      openPreview(event);
    }
  };

  return (
    <span
      class="message-image-preview"
      role="button"
      tabIndex={0}
      title={props.target.copyPath}
      onClick={openPreview}
      onContextMenu={openMenu}
      onKeyDown={handleKeyDown}
    >
      <Show
        when={!previewError() && previewSrc()}
        fallback={
          <span
            class="message-image-preview__placeholder"
            classList={{
              "message-image-preview__placeholder--loading": !previewError(),
            }}
          >
            <Show when={previewError()} fallback={
              <>
                <span class="message-image-preview__spinner" aria-hidden="true" />
                <span>Loading preview…</span>
              </>
            }>
              Preview unavailable
            </Show>
          </span>
        }
      >
        {(src) => (
          <img
            src={src()}
            alt={props.label || fileNameFromPath(props.target.path)}
            loading="lazy"
            draggable={false}
            onError={() => loadMarkdownImageDataUrl()}
          />
        )}
      </Show>
      <span class="message-image-preview__caption">
        {props.label || fileNameFromPath(props.target.path)}
      </span>
    </span>
  );
}

function childrenText(children: JSX.Element): string {
  if (children === null || children === undefined || typeof children === "boolean") {
    return "";
  }
  if (typeof children === "string" || typeof children === "number") {
    return String(children);
  }
  if (Array.isArray(children)) {
    return children.map(childrenText).join("");
  }
  return "";
}

function extractHastText(node: unknown): string {
  if (!node || typeof node !== "object") {
    return "";
  }
  const record = node as Record<string, unknown>;
  if (typeof record.value === "string") {
    return record.value;
  }
  if (!Array.isArray(record.children)) {
    return "";
  }
  return record.children.map(extractHastText).join("");
}

function joinClass(...parts: Array<string | undefined | false>) {
  return parts.filter(Boolean).join(" ") || undefined;
}
