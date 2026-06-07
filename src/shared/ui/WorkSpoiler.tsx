import { createSignal, type JSX, Show } from "solid-js";
import { ChevronDown } from "lucide-solid";

/**
 * A single collapsible block that hides a run's entire work — every tool call
 * and intermediate step — behind one header, leaving only the final answer
 * visible. Drives the "collapse work under a big spoiler" chat preference.
 * Generic over its children so the chat view passes real tool cards and the
 * settings preview passes a sample.
 */
export function WorkSpoiler(props: {
  label: string;
  count?: number;
  busy?: boolean;
  defaultOpen?: boolean;
  children: JSX.Element;
}) {
  const [open, setOpen] = createSignal(props.defaultOpen ?? false);

  return (
    <div
      classList={{
        "work-spoiler": true,
        "work-spoiler--open": open(),
        "work-spoiler--busy": Boolean(props.busy),
      }}
    >
      <button
        class="work-spoiler__head"
        type="button"
        aria-expanded={open()}
        onClick={() => setOpen((value) => !value)}
      >
        <Show
          when={props.busy}
          fallback={<ChevronDown class="work-spoiler__chevron" size={15} />}
        >
          <span class="work-spoiler__spinner" aria-hidden="true" />
        </Show>
        <span class="work-spoiler__title">{props.label}</span>
        <Show when={props.count != null}>
          <span class="work-spoiler__count">
            {props.count} {props.count === 1 ? "step" : "steps"}
          </span>
        </Show>
      </button>
      <Show when={open()}>
        <div class="work-spoiler__body">{props.children}</div>
      </Show>
    </div>
  );
}
