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
import { settingsCopy, type SettingsCopy } from "../settings-copy";
import {
  sectionMatches,
  SettingsHighlight,
  type SettingsSearchState,
} from "../settings-search";

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

export function ChatTab(props: { search: SettingsSearchState }) {
  const settings = () => chatSettings();
  const copy = () => settingsCopy().chat;
  const appearanceCopy = () => settingsCopy().appearance;
  const update = (patch: Partial<ChatSettings>) => updateChatSettings(patch);

  const sizeValue = () =>
    settings().fontSize === "inherit"
      ? CHAT_FONT_SIZE.default
      : (settings().fontSize as number);

  return (
    <div class="settings-pane__inner">
      <div class="settings-pane__intro">
        <h2>
          <SettingsHighlight text={copy().title} search={props.search} />
        </h2>
        <p>
          <SettingsHighlight text={copy().intro} search={props.search} />
        </p>
      </div>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "chat.preview",
          ),
        }}
        data-settings-section="chat.preview"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().previewTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().previewDescription}
              search={props.search}
            />
          </p>
        </div>
        <ChatPreview copy={settingsCopy()} />
      </section>

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "chat.message-font",
          ),
        }}
        data-settings-section="chat.message-font"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight
              text={copy().messageFontTitle}
              search={props.search}
            />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().messageFontDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="scope-grid">
          <label class="field-row">
            <span>{copy().font}</span>
            <select
              class="settings-select"
              value={settings().font}
              onChange={(event) =>
                update({
                  font: event.currentTarget.value as ChatSettings["font"],
                })
              }
            >
              <option value="inherit">{copy().sameAsInterface}</option>
              <For each={SANS_FONT_OPTIONS}>
                {(font) => (
                  <option value={font.id}>
                    {appearanceCopy().fontOptions[font.id] ?? font.label}
                  </option>
                )}
              </For>
            </select>
          </label>

          <div class="field-row">
            <span>{copy().messageSize}</span>
            <div class="tool-row" style={{ "border-radius": "9px" }}>
              <div class="tool-row__meta">
                <span class="tool-row__name">{copy().overrideSize}</span>
                <span class="tool-row__desc">
                  {settings().fontSize === "inherit"
                    ? copy().defaultSize
                    : copy().customSize(sizeValue())}
                </span>
              </div>
              <label class="switch" title={copy().overrideSize}>
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

      <section
        classList={{
          "settings-card": true,
          "settings-card--search-muted": !sectionMatches(
            props.search,
            "chat.layout",
          ),
        }}
        data-settings-section="chat.layout"
      >
        <div class="settings-card__head">
          <h3>
            <SettingsHighlight text={copy().layoutTitle} search={props.search} />
          </h3>
          <p>
            <SettingsHighlight
              text={copy().layoutDescription}
              search={props.search}
            />
          </p>
        </div>
        <div class="tool-list">
          <ToggleRow
            label={copy().hideAvatar}
            description={copy().hideAvatarDescription}
            checked={settings().hideAvatar}
            onChange={(value) => update({ hideAvatar: value })}
          />
          <ToggleRow
            label={copy().hideModelName}
            description={copy().hideModelNameDescription}
            checked={settings().hideModelName}
            onChange={(value) => update({ hideModelName: value })}
          />
          <ToggleRow
            label={copy().collapseWork}
            description={copy().collapseWorkDescription}
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

function ChatPreview(props: { copy: SettingsCopy }) {
  const collapse = () => chatSettings().collapseWork;

  return (
    <div class="chat-preview">
      <article class="message-row message-row--user">
        <div class="message-row__content">
          <div class="message-md">
            <p>
              {props.copy.nav.tabs.chat === "Чат"
                ? "Можешь отрефакторить auth module и запустить тесты?"
                : "Can you refactor the auth module and run the tests?"}
            </p>
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
            <span>{props.copy.nav.tabs.chat === "Чат" ? "только что" : "just now"}</span>
          </div>
          <div class="message-parts">
            <Show when={collapse()} fallback={<SampleTools />}>
              <WorkSpoiler
                label={
                  props.copy.nav.tabs.chat === "Чат"
                    ? "Работал 1м 12с"
                    : "Worked for 1m 12s"
                }
                count={SAMPLE_TOOLS.length}
              >
                <SampleTools />
              </WorkSpoiler>
            </Show>
            <div class="message-part message-part--text">
              <div class="message-md">
                <p>
                  {props.copy.nav.tabs.chat === "Чат" ? (
                    <>
                      Готово - вынес token logic в <code>auth/token.ts</code> и
                      обновил call sites. Все 42 теста проходят.
                    </>
                  ) : (
                    <>
                      Done - extracted the token logic into{" "}
                      <code>auth/token.ts</code> and updated the call sites. All
                      42 tests pass.
                    </>
                  )}
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
