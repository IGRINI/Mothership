// Full-screen image lightbox for tool/markdown image previews: zoom (wheel,
// buttons), pan (pointer drag), fullscreen, keyboard navigation, and a
// thumbnail strip when several images were opened together. Sources resolve
// through the asset protocol first, falling back to a data-url read.

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import {
  ChevronLeft,
  ChevronRight,
  Maximize2,
  Minimize2,
  RotateCcw,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-solid";
import { convertFileSrc } from "@tauri-apps/api/core";

import { readImageDataUrlWithTimeout } from "../../../shared/api/mothership";
import { isAbsoluteLocalFilePath } from "../file-references";
import { errorMessage } from "../message-model";
import type { ToolImagePreviewItem } from "../ToolCards";

export interface ImagePreviewState {
  items: ToolImagePreviewItem[];
  index: number;
}

const IMAGE_PREVIEW_MIN_ZOOM = 0.35;
const IMAGE_PREVIEW_MAX_ZOOM = 8;

function clampNumber(value: number, min: number, max: number) {
  return Math.min(Math.max(value, min), max);
}

export function ImagePreviewOverlay(props: {
  state: ImagePreviewState | null;
  onClose: () => void;
  onSelectIndex: (index: number) => void;
}) {
  let overlayRef: HTMLDivElement | undefined;
  let stageRef: HTMLDivElement | undefined;
  const [scale, setScale] = createSignal(1);
  const [offset, setOffset] = createSignal({ x: 0, y: 0 });
  const [drag, setDrag] = createSignal<
    | {
        pointerId: number;
        startX: number;
        startY: number;
        offsetX: number;
        offsetY: number;
      }
    | undefined
  >();
  const [isFullscreen, setIsFullscreen] = createSignal(false);
  const [activeSrc, setActiveSrc] = createSignal("");
  const [activeError, setActiveError] = createSignal("");
  let previewLoadRequestId = 0;
  // A press that begins on the empty backdrop (not the image) and ends without a
  // pan counts as "click outside" → close the preview.
  let downOnBackdrop = false;

  const activeImage = createMemo(() => {
    const state = props.state;
    if (!state || state.items.length === 0) {
      return null;
    }
    return state.items[clampNumber(state.index, 0, state.items.length - 1)];
  });
  const hasMultipleImages = () => (props.state?.items.length ?? 0) > 1;
  const zoomLabel = () => `${Math.round(scale() * 100)}%`;

  createEffect(() => {
    const image = activeImage();
    const requestId = ++previewLoadRequestId;
    setDrag(undefined);
    setScale(1);
    setOffset({ x: 0, y: 0 });
    const initialSrc =
      image?.src ||
      (image && isAbsoluteLocalFilePath(image.path)
        ? convertFileSrc(image.path)
        : "");
    setActiveSrc(initialSrc);
    setActiveError("");
    if (!image && document.fullscreenElement === overlayRef) {
      void document.exitFullscreen?.();
    }
    if (!image || initialSrc) {
      return;
    }
    loadActiveImageDataUrl(image, requestId);
  });

  onCleanup(() => {
    previewLoadRequestId += 1;
  });

  onMount(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (!props.state) {
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        props.onClose();
      } else if (event.key === "ArrowLeft" && hasMultipleImages()) {
        event.preventDefault();
        selectImage(props.state.index - 1);
      } else if (event.key === "ArrowRight" && hasMultipleImages()) {
        event.preventDefault();
        selectImage(props.state.index + 1);
      }
    };
    const handleFullscreenChange = () => {
      setIsFullscreen(document.fullscreenElement === overlayRef);
    };
    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("fullscreenchange", handleFullscreenChange);
    onCleanup(() => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("fullscreenchange", handleFullscreenChange);
    });
  });

  function selectImage(index: number) {
    const state = props.state;
    if (!state || state.items.length === 0) {
      return;
    }
    const nextIndex =
      ((index % state.items.length) + state.items.length) % state.items.length;
    props.onSelectIndex(nextIndex);
  }

  function resetView() {
    setDrag(undefined);
    setScale(1);
    setOffset({ x: 0, y: 0 });
  }

  function loadActiveImageDataUrl(
    image: ToolImagePreviewItem,
    requestId = ++previewLoadRequestId,
  ) {
    setActiveSrc("");
    setActiveError("");
    readImageDataUrlWithTimeout(undefined, image.path)
      .then((dataUrl) => {
        if (requestId === previewLoadRequestId) {
          setActiveSrc(dataUrl);
        }
      })
      .catch((error: unknown) => {
        if (requestId === previewLoadRequestId) {
          setActiveError(errorMessage(error));
        }
      });
  }

  function zoomBy(multiplier: number) {
    setScale((current) =>
      clampNumber(
        current * multiplier,
        IMAGE_PREVIEW_MIN_ZOOM,
        IMAGE_PREVIEW_MAX_ZOOM,
      ),
    );
  }

  function handleWheel(event: WheelEvent) {
    event.preventDefault();
    const currentScale = scale();
    const nextScale = clampNumber(
      currentScale * Math.exp(-event.deltaY * 0.0015),
      IMAGE_PREVIEW_MIN_ZOOM,
      IMAGE_PREVIEW_MAX_ZOOM,
    );
    const rect = stageRef?.getBoundingClientRect();
    if (rect) {
      const cursorX = event.clientX - rect.left - rect.width / 2;
      const cursorY = event.clientY - rect.top - rect.height / 2;
      const factor = nextScale / currentScale;
      setOffset((current) => ({
        x: cursorX - (cursorX - current.x) * factor,
        y: cursorY - (cursorY - current.y) * factor,
      }));
    }
    setScale(nextScale);
  }

  function handlePointerDown(event: PointerEvent) {
    if (!stageRef || event.button !== 0) {
      return;
    }
    downOnBackdrop = event.target === stageRef;
    event.preventDefault();
    stageRef.setPointerCapture(event.pointerId);
    const currentOffset = offset();
    setDrag({
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      offsetX: currentOffset.x,
      offsetY: currentOffset.y,
    });
  }

  function handlePointerMove(event: PointerEvent) {
    const currentDrag = drag();
    if (!currentDrag || currentDrag.pointerId !== event.pointerId) {
      return;
    }
    setOffset({
      x: currentDrag.offsetX + event.clientX - currentDrag.startX,
      y: currentDrag.offsetY + event.clientY - currentDrag.startY,
    });
  }

  function handlePointerUp(event: PointerEvent) {
    const currentDrag = drag();
    if (!currentDrag || currentDrag.pointerId !== event.pointerId) {
      return;
    }
    if (stageRef?.hasPointerCapture(event.pointerId)) {
      stageRef.releasePointerCapture(event.pointerId);
    }
    const movedDistance = Math.hypot(
      event.clientX - currentDrag.startX,
      event.clientY - currentDrag.startY,
    );
    setDrag(undefined);
    if (downOnBackdrop && movedDistance < 5) {
      props.onClose();
    }
  }

  async function toggleFullscreen() {
    if (!overlayRef) {
      return;
    }
    if (document.fullscreenElement === overlayRef) {
      await document.exitFullscreen?.();
      return;
    }
    await overlayRef.requestFullscreen?.();
  }

  return (
    <Show when={activeImage()}>
      {(image) => (
        <div
          ref={overlayRef}
          class="image-preview-overlay"
          role="dialog"
          aria-modal="true"
        >
          <div class="image-preview-toolbar">
            <div class="image-preview-toolbar__meta">
              <strong title={image().path}>{image().label}</strong>
              <Show when={hasMultipleImages()}>
                <span class="image-preview-toolbar__index">
                  {(props.state?.index ?? 0) + 1} / {props.state?.items.length ?? 1}
                </span>
              </Show>
            </div>
          </div>

          <div
            ref={stageRef}
            classList={{
              "image-preview-stage": true,
              "image-preview-stage--dragging": Boolean(drag()),
            }}
            onWheel={handleWheel}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={handlePointerUp}
            onPointerCancel={handlePointerUp}
            onLostPointerCapture={() => setDrag(undefined)}
            onDblClick={resetView}
          >
            <Show when={hasMultipleImages()}>
              <button
                type="button"
                class="image-preview-nav image-preview-nav--prev"
                aria-label="Previous image"
                title="Previous image"
                onPointerDown={(event) => event.stopPropagation()}
                onClick={() => selectImage((props.state?.index ?? 0) - 1)}
              >
                <ChevronLeft size={24} />
              </button>
              <button
                type="button"
                class="image-preview-nav image-preview-nav--next"
                aria-label="Next image"
                title="Next image"
                onPointerDown={(event) => event.stopPropagation()}
                onClick={() => selectImage((props.state?.index ?? 0) + 1)}
              >
                <ChevronRight size={24} />
              </button>
            </Show>
            <div
              class="image-preview-canvas"
              style={`--preview-scale: ${scale()}; --preview-x: ${offset().x}px; --preview-y: ${offset().y}px;`}
            >
              <Show
                when={!activeError() && activeSrc()}
                fallback={
                  <div class="image-preview-empty">
                    {activeError() ? "Preview unavailable" : "Loading preview..."}
                  </div>
                }
              >
                {(src) => (
                  <img
                    src={src()}
                    alt={image().label}
                    draggable={false}
                    onError={() => loadActiveImageDataUrl(image())}
                  />
                )}
              </Show>
            </div>
            <div
              class="image-preview-dock"
              onPointerDown={(event) => event.stopPropagation()}
            >
              <div class="image-preview-zoom">
                <button
                  type="button"
                  class="image-preview-zoom__btn"
                  title="Zoom out"
                  aria-label="Zoom out"
                  onClick={() => zoomBy(1 / 1.2)}
                >
                  <ZoomOut size={16} />
                </button>
                <span class="image-preview-zoom__value">{zoomLabel()}</span>
                <button
                  type="button"
                  class="image-preview-zoom__btn"
                  title="Zoom in"
                  aria-label="Zoom in"
                  onClick={() => zoomBy(1.2)}
                >
                  <ZoomIn size={16} />
                </button>
              </div>
              <button
                type="button"
                title="Reset"
                aria-label="Reset"
                onClick={resetView}
              >
                <RotateCcw size={16} />
              </button>
              <button
                type="button"
                title={isFullscreen() ? "Exit fullscreen" : "Fullscreen"}
                aria-label={isFullscreen() ? "Exit fullscreen" : "Fullscreen"}
                onClick={() => void toggleFullscreen()}
              >
                <Show when={isFullscreen()} fallback={<Maximize2 size={17} />}>
                  <Minimize2 size={17} />
                </Show>
              </button>
              <button
                type="button"
                class="image-preview-toolbar__close"
                title="Close"
                aria-label="Close"
                onClick={props.onClose}
              >
                <X size={18} />
              </button>
            </div>
          </div>

          <Show when={hasMultipleImages()}>
            <div class="image-preview-strip">
              <For each={props.state?.items ?? []}>
                {(item, index) => (
                  <button
                    type="button"
                    classList={{
                      "image-preview-strip__item": true,
                      "image-preview-strip__item--active":
                        index() === props.state?.index,
                    }}
                    title={item.path}
                    onClick={() => selectImage(index())}
                  >
                    <ImagePreviewStripThumb item={item} />
                  </button>
                )}
              </For>
            </div>
          </Show>
        </div>
      )}
    </Show>
  );
}

function ImagePreviewStripThumb(props: { item: ToolImagePreviewItem }) {
  const [src, setSrc] = createSignal(props.item.src);
  const [loadError, setLoadError] = createSignal("");
  let loadRequestId = 0;
  let triedDataUrl = false;

  createEffect(() => {
    const path = props.item.path;
    const initialSrc = props.item.src;
    loadRequestId += 1;
    triedDataUrl = false;
    setSrc(
      initialSrc ||
        (isAbsoluteLocalFilePath(path) ? convertFileSrc(path) : ""),
    );
    setLoadError("");
  });

  onCleanup(() => {
    loadRequestId += 1;
  });

  const loadDataUrlFallback = () => {
    const path = props.item.path;
    if (!path || triedDataUrl) {
      setSrc("");
      setLoadError("Preview unavailable");
      return;
    }

    triedDataUrl = true;
    const requestId = ++loadRequestId;
    setSrc("");
    setLoadError("");
    readImageDataUrlWithTimeout(undefined, path)
      .then((dataUrl) => {
        if (requestId === loadRequestId) {
          setSrc(dataUrl);
        }
      })
      .catch((error: unknown) => {
        if (requestId === loadRequestId) {
          setLoadError(errorMessage(error));
        }
      });
  };

  return (
    <Show
      when={!loadError() && src()}
      fallback={<span class="image-preview-strip__placeholder" />}
    >
      {(value) => (
        <img
          src={value()}
          alt={props.item.label}
          loading="lazy"
          onError={loadDataUrlFallback}
        />
      )}
    </Show>
  );
}
