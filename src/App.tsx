import { createSignal, Show } from "solid-js";

import { Dashboard } from "./features/dashboard/Dashboard";
import { Settings } from "./features/settings/Settings";
import { AppChrome } from "./shared/ui/AppChrome";
import "./App.css";

export default function App() {
  const [view, setView] = createSignal<"chat" | "settings">("chat");

  return (
    <AppChrome>
      <Show
        when={view() === "settings"}
        fallback={<Dashboard onOpenSettings={() => setView("settings")} />}
      >
        <Settings onBack={() => setView("chat")} />
      </Show>
    </AppChrome>
  );
}
