import { For, Show } from "solid-js";

import type {
  SettingsSearchDocument,
  SettingsTabId,
} from "./settings-copy";

export interface SettingsSearchResult extends SettingsSearchDocument {
  score: number;
}

export interface SettingsSearchState {
  active: boolean;
  query: string;
  terms: string[];
  results: SettingsSearchResult[];
  tabIds: Set<SettingsTabId>;
  sectionIds: Set<string>;
}

export const EMPTY_SETTINGS_SEARCH: SettingsSearchState = {
  active: false,
  query: "",
  terms: [],
  results: [],
  tabIds: new Set(),
  sectionIds: new Set(),
};

export function searchSettings(
  query: string,
  documents: SettingsSearchDocument[],
): SettingsSearchState {
  const terms = tokenize(query);
  if (terms.length === 0) {
    return EMPTY_SETTINGS_SEARCH;
  }

  const results = documents
    .map((document) => {
      const score = scoreDocument(document, terms);
      return score > 0 ? { ...document, score } : undefined;
    })
    .filter((item): item is SettingsSearchResult => Boolean(item))
    .sort((left, right) =>
      right.score === left.score
        ? left.title.localeCompare(right.title)
        : right.score - left.score,
    );

  return {
    active: true,
    query,
    terms,
    results,
    tabIds: new Set(results.map((result) => result.tabId)),
    sectionIds: new Set(results.map((result) => result.id)),
  };
}

export function sectionMatches(search: SettingsSearchState, sectionId: string) {
  return !search.active || search.sectionIds.has(sectionId);
}

export function tabMatches(search: SettingsSearchState, tabId: SettingsTabId) {
  return !search.active || search.tabIds.has(tabId);
}

export function SettingsHighlight(props: {
  text: string;
  search: SettingsSearchState;
}) {
  const parts = () => highlightedParts(props.text, props.search.terms);
  return (
    <Show when={props.search.active} fallback={props.text}>
      <For each={parts()}>
        {(part) => (
          <Show when={part.mark} fallback={part.text}>
            <mark class="settings-search-mark">{part.text}</mark>
          </Show>
        )}
      </For>
    </Show>
  );
}

function highlightedParts(text: string, terms: string[]) {
  const normalizedTerms = [...new Set(terms)]
    .filter((term) => term.length > 0)
    .sort((left, right) => right.length - left.length);
  if (normalizedTerms.length === 0) {
    return [{ text, mark: false }];
  }

  const pattern = new RegExp(
    `(${normalizedTerms.map(escapeRegExp).join("|")})`,
    "giu",
  );
  const out: { text: string; mark: boolean }[] = [];
  let lastIndex = 0;
  for (const match of text.matchAll(pattern)) {
    const index = match.index ?? 0;
    if (index > lastIndex) {
      out.push({ text: text.slice(lastIndex, index), mark: false });
    }
    out.push({ text: match[0], mark: true });
    lastIndex = index + match[0].length;
  }
  if (lastIndex < text.length) {
    out.push({ text: text.slice(lastIndex), mark: false });
  }
  return out.length > 0 ? out : [{ text, mark: false }];
}

function scoreDocument(document: SettingsSearchDocument, terms: string[]) {
  const title = normalize(document.title);
  const description = normalize(document.description);
  const keywordText = normalize(document.keywords.join(" "));
  const haystack = `${title} ${description} ${keywordText}`;
  let score = 0;

  for (const term of terms) {
    if (title === term) {
      score += 120;
    } else if (title.startsWith(term)) {
      score += 90;
    } else if (title.includes(term)) {
      score += 70;
    } else if (keywordText.includes(term)) {
      score += 50;
    } else if (description.includes(term)) {
      score += 30;
    } else if (haystackHasLooseToken(haystack, term)) {
      score += 12;
    } else {
      return 0;
    }
  }

  return score + Math.max(0, 16 - terms.length * 2);
}

function haystackHasLooseToken(haystack: string, term: string) {
  if (term.length < 3) {
    return false;
  }
  return tokenize(haystack).some((token) => isSubsequence(term, token));
}

function isSubsequence(needle: string, haystack: string) {
  let position = 0;
  for (const char of haystack) {
    if (char === needle[position]) {
      position += 1;
      if (position === needle.length) {
        return true;
      }
    }
  }
  return false;
}

function tokenize(value: string) {
  return normalize(value).match(/[\p{L}\p{N}]+/gu) ?? [];
}

function normalize(value: string) {
  return value
    .normalize("NFKD")
    .replace(/\p{Diacritic}/gu, "")
    .toLocaleLowerCase();
}

function escapeRegExp(value: string) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
