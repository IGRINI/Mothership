/* @refresh reload */
import { ErrorBoundary } from "solid-js";
import { render } from "solid-js/web";
import App from "./App";

const root = document.getElementById("root");

if (!root) {
  throw new Error("Mothership root element was not found.");
}

root.replaceChildren();

render(
  () => (
    <ErrorBoundary fallback={(error) => <StartupError error={error} />}>
      <App />
    </ErrorBoundary>
  ),
  root,
);

function StartupError(props: { error: unknown }) {
  const message =
    props.error instanceof Error
      ? props.error.message
      : "The interface failed to start.";
  const reload = () => {
    try {
      window.location.reload();
    } catch {
      window.location.href = window.location.href;
    }
  };

  return (
    <main class="startup-error" role="alert">
      <section class="startup-error__panel">
        <strong>Mothership could not start</strong>
        <span>{message}</span>
        <button class="startup-error__reload" type="button" onClick={reload}>
          Reload
        </button>
      </section>
    </main>
  );
}
