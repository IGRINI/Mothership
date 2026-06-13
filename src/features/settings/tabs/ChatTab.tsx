import { For, Show } from "solid-js";

import { SANS_FONT_OPTIONS } from "../../../shared/appearance";
import {
  CHAT_FONT_SIZE,
  chatSettings,
  updateChatSettings,
  type ChatSettings,
} from "../../../shared/chatSettings";
import { WorkSpoiler } from "../../../shared/ui/WorkSpoiler";
import { InlineToolCall } from "../../dashboard/components/InlineToolCall";
import { BrandMark } from "../../dashboard/components/BrandMark";
import type { ToolExecutionView } from "../../dashboard/types";

// Sample tool calls rendered through the REAL InlineToolCall component, so the
// preview is byte-for-byte the same as the live chat (icon + headline + chevron,
// the same collapsed spoiler), never a hand-rolled lookalike.
const SAMPLE_TOOLS: ToolExecutionView[] = [
  {
    toolCallId: "preview-read",
    kind: "completed",
    toolKind: "read_file",
    payload: { path: "auth/session.ts" },
    output: "",
    createdAt: 0,
    updatedAt: 0,
  },
  {
    toolCallId: "preview-edit",
    kind: "completed",
    toolKind: "edit_file",
    payload: { path: "auth/token.ts" },
    output: "",
    createdAt: 0,
    updatedAt: 0,
  },
  {
    toolCallId: "preview-run",
    kind: "completed",
    toolKind: "run_command",
    command: { program: "npm", args: ["test"], env: {} },
    output: "",
    createdAt: 0,
    updatedAt: 0,
  },
];

/**
 * Chat tab: how chat messages render — message font/size (inherit from the
 * global appearance or override), and toggles for the provider avatar, the
 * model name, and collapsing the run's work under one spoiler. A live sample
 * reflects every change instantly (the prefs apply to the document root, so the
 * sample — built from the real message classes — re-skins with the real chat).
 */
export function ChatTab() {
  const settings = () => chatSettings();
  const update = (patch: Partial<ChatSettings>) => updateChatSettings(patch);

  const sizeValue = () =>
    settings().fontSize === "inherit"
      ? CHAT_FONT_SIZE.default
      : (settings().fontSize as number);

  return (
    <div class="settings-pane__inner">
      <div class="settings-pane__intro">
        <h2>Chat</h2>
        <p>
          Control how chat messages look. These preferences are stored on this
          device and apply live — the example below updates as you change them.
        </p>
      </div>

      {/* Live preview */}
      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Preview</h3>
          <p>A sample exchange rendered with your current settings.</p>
        </div>
        <ChatPreview />
      </section>

      {/* Typography */}
      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Message font</h3>
          <p>Use the interface font from Appearance, or pick a different one.</p>
        </div>
        <div class="scope-grid">
          <label class="field-row">
            <span>Font</span>
            <select
              class="settings-select"
              value={settings().font}
              onChange={(event) =>
                update({
                  font: event.currentTarget.value as ChatSettings["font"],
                })
              }
            >
              <option value="inherit">Same as interface</option>
              <For each={SANS_FONT_OPTIONS}>
                {(font) => <option value={font.id}>{font.label}</option>}
              </For>
            </select>
          </label>

          <div class="field-row">
            <span>Message size</span>
            <div class="tool-row" style={{ "border-radius": "9px" }}>
              <div class="tool-row__meta">
                <span class="tool-row__name">Override size</span>
                <span class="tool-row__desc">
                  {settings().fontSize === "inherit"
                    ? "Using the default chat size."
                    : `Custom: ${sizeValue()}px`}
                </span>
              </div>
              <label class="switch" title="Override message size">
                <input
                  type="checkbox"
                  checked={settings().fontSize !== "inherit"}
                  onChange={(event) =>
                    update({
                      fontSize: event.currentTarget.checked
                        ? CHAT_FONT_SIZE.default
                        : "inherit",
                    })
                  }
                />
                <span class="switch__track" />
                <span class="switch__thumb" />
              </label>
            </div>
            {/* Reserve the slider's row whether or not it's shown, so toggling
                the override never shifts anything below it. */}
            <div class="size-slider-slot">
              <Show when={settings().fontSize !== "inherit"}>
                <div class="scale-control__row">
                  <input
                    type="range"
                    min={CHAT_FONT_SIZE.min}
                    max={CHAT_FONT_SIZE.max}
                    step={CHAT_FONT_SIZE.step}
                    value={sizeValue()}
                    onInput={(event) =>
                      update({ fontSize: Number(event.currentTarget.value) })
                    }
                  />
                  <span class="scale-value">{sizeValue()}px</span>
                </div>
              </Show>
            </div>
          </div>
        </div>
      </section>

      {/* Message chrome */}
      <section class="settings-card">
        <div class="settings-card__head">
          <h3>Message layout</h3>
          <p>Trim the assistant byline to taste.</p>
        </div>
        <div class="tool-list">
          <ToggleRow
            label="Hide provider avatar"
            description="Drop the provider icon next to assistant replies."
            checked={settings().hideAvatar}
            onChange={(value) => update({ hideAvatar: value })}
          />
          <ToggleRow
            label="Hide model name"
            description="Drop the model label above each assistant reply."
            checked={settings().hideModelName}
            onChange={(value) => update({ hideModelName: value })}
          />
          <ToggleRow
            label="Collapse work under a spoiler"
            description="Hide every tool call and intermediate step behind one collapsible header, leaving only the final answer."
            checked={settings().collapseWork}
            onChange={(value) => update({ collapseWork: value })}
          />
        </div>
      </section>
    </div>
  );
}

function ToggleRow(props: {
  label: string;
  description: string;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <div class="tool-row">
      <div class="tool-row__meta">
        <span class="tool-row__name">{props.label}</span>
        <span class="tool-row__desc">{props.description}</span>
      </div>
      <label class="switch" title={props.label}>
        <input
          type="checkbox"
          checked={props.checked}
          onChange={(event) => props.onChange(event.currentTarget.checked)}
        />
        <span class="switch__track" />
        <span class="switch__thumb" />
      </label>
    </div>
  );
}

/** Sample conversation built from the REAL chat components/classes (Avatar mark,
 * message rows, InlineToolCall, WorkSpoiler), so it's identical to the live chat
 * and reflects every global chat pref (font/size/hide attributes). */
function ChatPreview() {
  const collapse = () => chatSettings().collapseWork;

  return (
    <div class="chat-preview">
      <article class="message-row message-row--user">
        <div class="message-row__content">
          <div class="message-md">
            <p>Can you refactor the auth module and run the tests?</p>
          </div>
        </div>
      </article>

      <article class="message-row message-row--assistant">
        <div class="avatar avatar--message avatar--agent">
          <BrandMark compact />
        </div>
        <div class="message-row__content">
          <div class="message-meta">
            <strong>GPT-5.5</strong>
            <span>just now</span>
          </div>
          <div class="message-parts">
            <Show when={collapse()} fallback={<SampleTools />}>
              <WorkSpoiler label="Worked for 1m 12s" count={SAMPLE_TOOLS.length}>
                <SampleTools />
              </WorkSpoiler>
            </Show>
            <div class="message-part message-part--text">
              <div class="message-md">
                <p>
                  Done — extracted the token logic into <code>auth/token.ts</code>{" "}
                  and updated the call sites. All 42 tests pass.
                </p>
              </div>
            </div>
          </div>
        </div>
      </article>
    </div>
  );
}

function SampleTools() {
  return (
    <For each={SAMPLE_TOOLS}>
      {(tool) => (
        <div class="message-part message-part--tool">
          <InlineToolCall
            expanded={false}
            tool={tool}
            onApprove={() => {}}
            onCancel={() => {}}
            onDeny={() => {}}
            onPreviewImage={() => {}}
            onToggle={() => {}}
          />
        </div>
      )}
    </For>
  );
}
