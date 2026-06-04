export function formatRelativeTime(timestamp: string) {
  const seconds = Math.max(
    0,
    Math.floor((Date.now() - unixTimestampToDate(timestamp).getTime()) / 1000),
  );

  if (seconds < 60) {
    return "now";
  }

  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m ago`;
  }

  if (seconds < 86_400) {
    return `${Math.floor(seconds / 3600)}h ago`;
  }

  return unixTimestampToDate(timestamp).toLocaleDateString([], {
    month: "short",
    day: "numeric",
  });
}

function unixTimestampToDate(timestamp: string) {
  const numericTimestamp = Number(timestamp);
  return new Date(
    Number.isFinite(numericTimestamp) ? numericTimestamp * 1000 : Date.now(),
  );
}
