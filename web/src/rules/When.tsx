import { formatDateTime, formatUtc } from "../format";

/** A time: local first, UTC beside it. */
export function When({ ms }: { ms: number }) {
  return (
    <>
      <time dateTime={new Date(ms).toISOString()}>{formatDateTime(ms)}</time> <small className="muted">{formatUtc(ms)}</small>
    </>
  );
}
