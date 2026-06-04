import mothershipLogoUrl from "../../../assets/mothership-logo-sm.png";

export function BrandMark(props: { compact?: boolean }) {
  return (
    <span
      classList={{
        "brand-mark": true,
        "brand-mark--compact": Boolean(props.compact),
      }}
      aria-hidden="true"
    >
      <img src={mothershipLogoUrl} alt="" draggable={false} />
    </span>
  );
}
