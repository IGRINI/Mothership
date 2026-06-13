// Generic searchable dropdown used by the header model/provider selectors:
// trigger button + popover with a filter input, keyboard navigation, and an
// optional "create from query" row (custom model ids).

import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Check, ChevronDown, Search } from "lucide-solid";

import type { SearchSelectOption } from "../types";

export function SearchSelect(props: {
  ariaLabel: string;
  class?: string;
  emptyLabel: string;
  options: SearchSelectOption[];
  placeholder: string;
  title?: string;
  value?: string;
  createOption?: (query: string) => SearchSelectOption | undefined;
  onSelect: (value: string) => void;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [activeIndex, setActiveIndex] = createSignal(-1);
  let rootRef: HTMLDivElement | undefined;
  let inputRef: HTMLInputElement | undefined;

  const selectedOption = () =>
    props.options.find((option) => option.value === props.value);
  const emptyLabel = () =>
    query().trim().length > 0 ? "No matches" : props.emptyLabel;
  const filteredOptions = createMemo(() => {
    const normalizedQuery = normalizeSearchQuery(query());
    const filtered = normalizedQuery
      ? props.options.filter((option) =>
          normalizeSearchQuery(
            [option.label, option.detail, option.searchText, option.status?.label]
              .filter(Boolean)
              .join(" "),
          ).includes(normalizedQuery),
        )
      : props.options;
    const created = normalizedQuery ? props.createOption?.(query().trim()) : undefined;
    if (!created || filtered.some((option) => option.value === created.value)) {
      return filtered;
    }
    return [...filtered, created];
  });

  createEffect(() => {
    if (!isOpen()) {
      return;
    }

    const options = filteredOptions();
    const current = activeIndex();
    if (current >= 0 && current < options.length && !options[current]?.disabled) {
      return;
    }

    setActiveIndex(firstSelectableOptionIndex(options));
  });

  onMount(() => {
    const handlePointerDown = (event: PointerEvent) => {
      if (!isOpen() || !rootRef) {
        return;
      }

      if (event.target instanceof Node && !rootRef.contains(event.target)) {
        closeDropdown();
      }
    };

    document.addEventListener("pointerdown", handlePointerDown);
    onCleanup(() => {
      document.removeEventListener("pointerdown", handlePointerDown);
    });
  });

  const openDropdown = () => {
    setIsOpen(true);
    setQuery("");
    setActiveIndex(firstSelectableOptionIndex(filteredOptions()));
    window.setTimeout(() => inputRef?.focus(), 0);
  };
  const closeDropdown = () => {
    setIsOpen(false);
    setQuery("");
    setActiveIndex(-1);
  };
  const toggleDropdown = () => {
    if (isOpen()) {
      closeDropdown();
    } else {
      openDropdown();
    }
  };
  const selectOption = (option: SearchSelectOption) => {
    if (option.disabled) {
      return;
    }

    props.onSelect(option.value);
    closeDropdown();
  };
  const moveActiveOption = (delta: number) => {
    const options = filteredOptions();
    if (options.length === 0) {
      setActiveIndex(-1);
      return;
    }

    let nextIndex = activeIndex();
    for (let attempts = 0; attempts < options.length; attempts += 1) {
      nextIndex = (nextIndex + delta + options.length) % options.length;
      if (!options[nextIndex]?.disabled) {
        setActiveIndex(nextIndex);
        return;
      }
    }

    setActiveIndex(-1);
  };
  const selectActiveOption = () => {
    const option = filteredOptions()[activeIndex()];
    if (option) {
      selectOption(option);
    }
  };
  const handleTriggerKeyDown = (event: KeyboardEvent) => {
    if (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      openDropdown();
    }
  };
  const handleSearchKeyDown = (event: KeyboardEvent) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveActiveOption(1);
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveActiveOption(-1);
    }
    if (event.key === "Enter") {
      event.preventDefault();
      selectActiveOption();
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeDropdown();
    }
  };

  return (
    <div class={`search-select ${props.class ?? ""}`} ref={rootRef}>
      <button
        class="search-select__trigger"
        type="button"
        aria-expanded={isOpen()}
        aria-haspopup="listbox"
        aria-label={props.ariaLabel}
        title={props.title ?? selectedOption()?.label ?? props.placeholder}
        onClick={toggleDropdown}
        onKeyDown={handleTriggerKeyDown}
      >
        <span class="search-select__value">
          {selectedOption()?.label ?? props.placeholder}
        </span>
        <ChevronDown
          classList={{
            "search-select__chevron": true,
            "search-select__chevron--open": isOpen(),
          }}
          size={14}
        />
      </button>

      <Show when={isOpen()}>
        <div class="search-select__popover">
          <label class="search-select__search">
            <Search size={13} />
            <input
              ref={inputRef}
              aria-label={`Search ${props.ariaLabel.toLowerCase()}`}
              autocomplete="off"
              spellcheck={false}
              placeholder="Search..."
              value={query()}
              onInput={(event) => setQuery(event.currentTarget.value)}
              onKeyDown={handleSearchKeyDown}
            />
          </label>

          <div class="search-select__list" role="listbox">
            <For
              each={filteredOptions()}
              fallback={<div class="search-select__empty">{emptyLabel()}</div>}
            >
              {(option, index) => (
                <button
                  classList={{
                    "search-select__option": true,
                    "search-select__option--active": index() === activeIndex(),
                    "search-select__option--selected": option.value === props.value,
                  }}
                  type="button"
                  role="option"
                  aria-selected={option.value === props.value}
                  disabled={option.disabled}
                  title={option.title}
                  onMouseEnter={() => {
                    if (!option.disabled) {
                      setActiveIndex(index());
                    }
                  }}
                  onClick={() => selectOption(option)}
                >
                  <span class="search-select__option-text">
                    <span class="search-select__option-label">{option.label}</span>
                    <Show when={option.detail}>
                      <span class="search-select__option-detail">
                        {option.detail}
                      </span>
                    </Show>
                  </span>
                  <Show when={option.status}>
                    <span
                      class={`search-select__option-status search-select__option-status--${option.status!.tone}`}
                    >
                      {option.status!.label}
                    </span>
                  </Show>
                  <Show when={option.value === props.value}>
                    <Check class="search-select__check" size={14} />
                  </Show>
                </button>
              )}
            </For>
          </div>
        </div>
      </Show>
    </div>
  );
}

function firstSelectableOptionIndex(options: SearchSelectOption[]) {
  return options.findIndex((option) => !option.disabled);
}

function normalizeSearchQuery(value: string) {
  return value.trim().toLowerCase();
}
