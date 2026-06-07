// Per-chat, per-model reasoning. Each chat stores a map { "<providerId>/<modelId>"
// → reasoningOptionId } so switching the chat's model restores that model's
// chosen reasoning, independently per chat. The map is serialized as JSON into
// the chat's `reasoning` field (the backend treats it as an opaque string and
// copies it when a new chat is created — so a new chat inherits the whole map).

export type ReasoningMap = Record<string, string>;

export function modelKey(providerId: string, modelId: string): string {
  return `${providerId}/${modelId}`;
}

/** Parse a chat's serialized reasoning map (tolerant of legacy single-option
 * values and malformed JSON — both yield an empty map). */
export function parseReasoningMap(serialized: string | null | undefined): ReasoningMap {
  if (!serialized) {
    return {};
  }
  try {
    const parsed = JSON.parse(serialized) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as ReasoningMap;
    }
    return {};
  } catch {
    return {};
  }
}

/** Serialize a reasoning map for storage, or null when empty (so the column
 * stays NULL rather than "{}"). */
export function serializeReasoningMap(map: ReasoningMap): string | null {
  return Object.keys(map).length > 0 ? JSON.stringify(map) : null;
}
