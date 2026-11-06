// Formatting helpers. Sizes use binary units (KiB, MiB, …), which is what file managers on
// every platform agree on for disk sizes.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "–";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit++;
  }
  if (unit === 0) return `${value} B`;
  const digits = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

export function formatRate(bytesPerSecond: number): string {
  if (bytesPerSecond <= 0) return "";
  return `${formatBytes(bytesPerSecond)}/s`;
}

export function formatPercent(fraction: number): string {
  const pct = Math.max(0, Math.min(1, fraction)) * 100;
  if (pct === 100) return "100%";
  // Never show 100% for something that isn't finished.
  return `${Math.min(pct, 99.9).toFixed(1)}%`;
}

/** Seconds remaining, as a short human duration: `45s`, `12m`, `3h 05m`, `2d 4h`. */
export function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "∞";
  const s = Math.round(seconds);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${String(m % 60).padStart(2, "0")}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

export function formatRatio(uploaded: number, downloaded: number): string {
  if (downloaded <= 0) return uploaded > 0 ? "∞" : "0.00";
  return (uploaded / downloaded).toFixed(2);
}

/** A local date and time, in the user's own locale and time zone. */
export function formatDateTime(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

/** The same instant in UTC, for tooltips. */
export function formatUtc(ms: number): string {
  return `${new Date(ms).toISOString().slice(0, 16).replace("T", " ")}Z`;
}
