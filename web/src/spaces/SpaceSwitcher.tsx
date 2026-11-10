// The header's space switcher. Hidden while the caller has a single space, so an install
// with one space looks as it did before spaces. Switching loads the other space's pages
// (`/s/<slug>/`): its torrents, sync socket and settings are all different.

import { spaceHome } from "./prefix";
import { useSpaceContext } from "./context";

export function SpaceSwitcher() {
  const ctx = useSpaceContext();
  if (!ctx || ctx.spaces.length < 2) return null;
  return (
    <label className="space-switcher">
      <select
        aria-label="Space"
        value={ctx.current.slug}
        onChange={(e) => {
          location.assign(spaceHome(e.target.value));
        }}
      >
        {ctx.spaces.map((s) => (
          <option key={s.id} value={s.slug} title={s.description || undefined}>
            {s.name}
          </option>
        ))}
      </select>
    </label>
  );
}
