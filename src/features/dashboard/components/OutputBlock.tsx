import { Show, createSignal } from "solid-js";

const VISIBLE_CAP = 10;

/**
 * A plain monospace output block (command stdout/stderr, search hits, file
 * listings) with a 10-line cap and an instant "ещё N строк" reveal. The text is
 * already bounded by the backend's output policy, so the reveal is in-memory —
 * nothing is fetched.
 */
export function OutputBlock(props: { text: string; tone?: "out" | "err" }) {
  const [showAll, setShowAll] = createSignal(false);
  const lines = () =>
    props.text.replace(/\r\n/g, "\n").replace(/\n+$/, "").split("\n");
  const visible = () =>
    showAll() ? lines() : lines().slice(0, VISIBLE_CAP);
  const hidden = () =>
    showAll() ? 0 : Math.max(0, lines().length - VISIBLE_CAP);

  return (
    <div class="output-block">
      <pre
        classList={{
          "output-block__body": true,
          "output-block__body--err": props.tone === "err",
        }}
      >
        {visible().join("\n")}
      </pre>
      <Show when={hidden() > 0}>
        <button
          class="code-block__more"
          type="button"
          onClick={() => setShowAll(true)}
        >
          … ещё {hidden()} строк
        </button>
      </Show>
    </div>
  );
}
