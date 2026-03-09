const tierColors: Record<string, string> = {
  light: "bg-sky-900/60 text-sky-300",
  standard: "bg-amber-900/60 text-amber-300",
  heavy: "bg-purple-900/60 text-purple-300",
};

export default function TierBadge(props: { tier: string }) {
  const color = () => tierColors[props.tier] ?? "bg-button-bg text-text";
  return (
    <span
      class={`inline-block rounded px-1.5 py-0.5 text-[10px] font-medium uppercase leading-none ${color()}`}
    >
      {props.tier}
    </span>
  );
}
