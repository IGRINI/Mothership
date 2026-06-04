import type { JSX } from "solid-js";

export function SectionHeader(props: { action?: JSX.Element; title: string }) {
  return (
    <div class="section-header">
      <span>{props.title}</span>
      {props.action}
    </div>
  );
}
