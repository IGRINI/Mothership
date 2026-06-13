import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { RefreshCw } from "lucide-solid";

import {
  getSidecarHealth,
  onSidecarStatus,
  restartSidecar,
  type SidecarStatusEvent,
} from "../api/mothership";

type GuardState = "ok" | "reconnecting" | "failed";

/**
 * Full-screen takeover shown when the agent core (sidecar) dies. Transient
 * outages show a reconnect spinner while the host auto-restarts with backoff;
 * a permanent failure (restart budget exhausted) offers a manual restart.
 * Once the core comes back after ANY outage the webview reloads — every event
 * stream was severed while it was down, so a full re-bootstrap (projects,
 * chats, session restore, run recovery) is the only honest resync.
 */
export function SidecarGuard() {
  const [state, setState] = createSignal<GuardState>("ok");
  const [attempt, setAttempt] = createSignal<string | null>(null);
  const [detail, setDetail] = createSignal<string | null>(null);
  const [restarting, setRestarting] = createSignal(false);
  let sawOutage = false;
  let disposed = false;
  let unlisten: (() => void) | undefined;

  const applyStatus = (status: SidecarStatusEvent) => {
    if (status.state === "ready") {
      if (sawOutage) {
        window.location.reload();
        return;
      }
      setState("ok");
      return;
    }
    sawOutage = true;
    if (status.detail) {
      setDetail(status.detail);
    }
    setAttempt(
      status.state === "down" && status.attempt
        ? `attempt ${status.attempt} of ${status.maxAttempts ?? "?"}`
        : null,
    );
    setState(
      status.permanent || status.state === "failed" ? "failed" : "reconnecting",
    );
  };

  onMount(() => {
    void onSidecarStatus(applyStatus).then((dispose) => {
      if (disposed) {
        dispose();
      } else {
        unlisten = dispose;
      }
    });
    // Seed from current health — a crash that happened before our listener
    // registered would otherwise go unnoticed. Boot-time "starting" is normal
    // launch progress, not an outage, so only down/failed seed the overlay.
    void getSidecarHealth()
      .then((status) => {
        if (status.state === "down" || status.state === "failed") {
          applyStatus(status);
        }
      })
      .catch(() => {});
  });

  onCleanup(() => {
    disposed = true;
    unlisten?.();
  });

  const handleRestart = () => {
    if (restarting()) {
      return;
    }
    setRestarting(true);
    setDetail(null);
    restartSidecar()
      .then(() => window.location.reload())
      .catch((error: unknown) => {
        setDetail(error instanceof Error ? error.message : String(error));
        setRestarting(false);
      });
  };

  return (
    <Show when={state() !== "ok"}>
      <div class="sidecar-overlay" role="alertdialog" aria-modal="true">
        <div class="sidecar-overlay__card">
          <Show
            when={state() === "failed"}
            fallback={
              <>
                <div class="sidecar-overlay__spinner" aria-hidden="true" />
                <h2>Reconnecting to the agent core…</h2>
                <p>
                  The core process stopped and is being restarted
                  {attempt() ? ` (${attempt()})` : ""}. Running agents were
                  interrupted; their chats will show what happened.
                </p>
              </>
            }
          >
            <h2>The agent core stopped</h2>
            <p>
              Automatic restarts didn't bring it back. Your chats, drafts and
              change history are safe on disk — restart the core to continue
              where you left off.
            </p>
            <button
              class="sidecar-overlay__restart"
              type="button"
              disabled={restarting()}
              onClick={handleRestart}
            >
              <RefreshCw size={15} />
              {restarting() ? "Restarting…" : "Restart core"}
            </button>
          </Show>
          <Show when={detail()}>
            <pre class="sidecar-overlay__detail">{detail()}</pre>
          </Show>
        </div>
      </div>
    </Show>
  );
}
