// Composer reasoning-level menu: offers the active model's reasoning options
// (Авто / Низкий / … per the adapter's declared set) and reports the pick.
// Disabled entirely for models without configurable reasoning.

import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { BrainCircuit, Check, ChevronDown } from "lucide-solid";

import type { LlmModel } from "../../../shared/api/mothership";
import {
  reasoningSelectorOptions,
  type ReasoningSelectorOption,
} from "../model-options";
import type { ReasoningOptionId } from "./Composer";

export function ReasoningSelector(props: {
  disabled: boolean;
  model?: LlmModel;
  value?: ReasoningOptionId;
  onChange: (optionId: ReasoningOptionId) => void;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  const [activeIndex, setActiveIndex] = createSignal(-1);
  let rootRef: HTMLDivElement | undefined;

  const options = createMemo(() => reasoningSelectorOptions(props.model));
  const supported = () => options().length > 0;
  const selectedOption = () =>
    options().find((option) => option.value === props.value) ??
    options().find((option) => option.option.recommended) ??
    options()[0];

  createEffect(() => {
    if (!isOpen()) {
      return;
    }

    const currentOptions = options();
    const current = activeIndex();
    if (current >= 0 && current < currentOptions.length) {
      return;
    }

    setActiveIndex(firstSelectableReasoningOptionIndex(currentOptions));
  });

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }

      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        closeMenu();
      }
    };

    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", handlePointerDown);
    });
  });

  const closeMenu = () => {
    setIsOpen(false);
    setActiveIndex(-1);
  };
  const openMenu = () => {
    if (props.disabled || !supported()) {
      return;
    }

    setIsOpen(true);
    setActiveIndex(firstSelectableReasoningOptionIndex(options()));
  };
  const toggleMenu = () => {
    if (isOpen()) {
      closeMenu();
    } else {
      openMenu();
    }
  };
  const selectOption = (option: ReasoningSelectorOption) => {
    props.onChange(option.value);
    closeMenu();
  };
  const moveActiveOption = (delta: number) => {
    const currentOptions = options();
    if (currentOptions.length === 0) {
      setActiveIndex(-1);
      return;
    }

    let nextIndex = activeIndex();
    for (let attempts = 0; attempts < currentOptions.length; attempts += 1) {
      nextIndex =
        (nextIndex + delta + currentOptions.length) % currentOptions.length;
      if (currentOptions[nextIndex]) {
        setActiveIndex(nextIndex);
        return;
      }
    }

    setActiveIndex(-1);
  };
  const selectActiveOption = () => {
    const option = options()[activeIndex()];
    if (option) {
      selectOption(option);
    }
  };
  const handleTriggerKeyDown = (event: KeyboardEvent) => {
    if (!isOpen()) {
      if (
        event.key === "ArrowDown" ||
        event.key === "Enter" ||
        event.key === " "
      ) {
        event.preventDefault();
        openMenu();
      }
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveActiveOption(1);
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveActiveOption(-1);
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      selectActiveOption();
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeMenu();
    }
  };

  return (
    <div
      ref={rootRef}
      classList={{
        "reasoning-selector": true,
        "reasoning-selector--disabled": !supported(),
      }}
    >
      <button
        class="reasoning-selector__trigger"
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="menu"
        disabled={props.disabled || !supported()}
        title={
          supported()
            ? "Рассуждение для следующего сообщения"
            : "Выбранная модель не поддерживает настройку рассуждения"
        }
        onClick={toggleMenu}
        onKeyDown={handleTriggerKeyDown}
      >
        <BrainCircuit size={15} />
        <span>{selectedOption()?.label ?? "Рассуждение"}</span>
        <ChevronDown
          classList={{
            "reasoning-selector__chevron": true,
            "reasoning-selector__chevron--open": isOpen(),
          }}
          size={13}
        />
      </button>

      <Show when={isOpen()}>
        <div class="reasoning-selector__popover" role="menu">
          <div class="reasoning-selector__heading">Рассуждение</div>
          <For each={options()}>
            {(option, index) => (
              <button
                classList={{
                  "reasoning-selector__item": true,
                  "reasoning-selector__item--active": index() === activeIndex(),
                  "reasoning-selector__item--selected":
                    option.value === selectedOption()?.value,
                }}
                type="button"
                role="menuitemradio"
                aria-checked={option.value === selectedOption()?.value}
                title={option.title}
                onMouseEnter={() => setActiveIndex(index())}
                onClick={() => selectOption(option)}
              >
                <span>{option.label}</span>
                <Show when={option.value === selectedOption()?.value}>
                  <Check size={14} />
                </Show>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function firstSelectableReasoningOptionIndex(options: ReasoningSelectorOption[]) {
  return options.length > 0 ? 0 : -1;
}
