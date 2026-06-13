import { createEffect, createSignal, Show } from "solid-js";

import { Dashboard } from "./features/dashboard/Dashboard";
import { Settings } from "./features/settings/Settings";
import { agentFocusRequest, initAgentActivity } from "./shared/agentActivity";
import { AppChrome } from "./shared/ui/AppChrome";
import { ContextMenu } from "./shared/ui/ContextMenu";
import { SidecarGuard } from "./shared/ui/SidecarGuard";
import "./App.css";

export default function App() {
  const [view, setView] = createSignal<"chat" | "settings">("chat");

  // App-wide agent tracking behind the status-bar pill (runs across ALL
  // projects). Started here so it observes runs even while Settings is open.
  initAgentActivity();

  // A click on an agent in the status pill must land in the workspace; the
  // request itself is consumed by Dashboard once it is (re)mounted.
  createEffect(() => {
    if (agentFocusRequest()) {
      setView("chat");
    }
  });

  return (
    <AppChrome onOpenSettings={() => setView("settings")}>
      <Show
        when={view() === "settings"}
        fallback={<Dashboard />}
      >
        <Settings onBack={() => setView("chat")} />
      </Show>
      {/* App-wide custom right-click menu (file actions, etc.). */}
      <ContextMenu />
      {/* Full-screen takeover when the agent core dies (manual restart). */}
      <SidecarGuard />
    </AppChrome>
  );
}
