// Settings sections that act at once instead of through the Save bar: banned peers, the
// Deluge import, and this device's appearance.

import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { Ban } from "../api/generated/Ban";
import { DelugeTakeOverSection } from "../components/DelugeTakeOverSection";
import { formatDateTime } from "../format";
import { setDetailsMode, useDetailsMode, type DetailsMode } from "../state/details";
import { THEME_PREFERENCES, setTheme, useTheme, type ThemePreference } from "../theme";
import { ChoiceCards, Empty, SettingCard, type Choice } from "./controls";

const BAN_REASONS: Record<Ban["reason"], string> = {
  badData: "Sent data that failed verification",
  untrusted: "In too many failed pieces",
  manual: "Banned by hand",
};

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function BansSection({ mayChange }: { mayChange: boolean }) {
  const [bans, setBans] = useState<Ban[] | null>(null);
  const [ip, setIp] = useState("");
  const [error, setError] = useState<string | null>(null);
  const load = () => {
    api.bans().then(setBans, (e: unknown) => setError(message(e)));
  };
  useEffect(load, []);
  return (
    <SettingCard
      description="Peers are banned for a while when they send data that fails the torrent's hashes. Bans and unbans apply at once."
      footer={
        mayChange && (
          <form
            className="s-add"
            onSubmit={(e) => {
              e.preventDefault();
              void api.ban(ip.trim()).then(
                () => {
                  setIp("");
                  setError(null);
                  load();
                },
                (err: unknown) => setError(message(err)),
              );
            }}
          >
            <input aria-label="Address to ban" value={ip} placeholder="IP address" spellCheck={false} onChange={(e) => setIp(e.target.value)} />
            <button type="submit" disabled={ip.trim() === ""}>
              Ban
            </button>
          </form>
        )
      }
    >
      {bans?.length === 0 && <Empty>Nobody is banned.</Empty>}
      {bans && bans.length > 0 && (
        <ul className="s-list">
          {bans.map((b) => (
            <li key={b.ip}>
              <div className="s-list-main">
                <code>{b.ip}</code>
                <span className="muted">
                  {BAN_REASONS[b.reason]}
                  {b.count > 1 && ` (${b.count} times)`} · until {formatDateTime(b.until)}
                </span>
              </div>
              {mayChange && (
                <div className="s-list-actions">
                  <button type="button" onClick={() => void api.unban(b.ip).then(load, (e: unknown) => setError(message(e)))}>
                    Unban
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      {error && <p className="error-text">{error}</p>}
    </SettingCard>
  );
}

export function DelugeSection({ onImport }: { onImport: () => void }) {
  return (
    <>
      <SettingCard
        title="Import from Deluge"
        description="A wizard that finds the Deluge installations on this server, lists their torrents with filters, and imports the ones you pick into a space, or a new one. Deluge can keep running: each torrent runs in only one client at a time."
      >
        <div>
          <button type="button" className="primary" onClick={onImport}>
            Import from Deluge…
          </button>
        </div>
      </SettingCard>
      <DelugeTakeOverSection />
    </>
  );
}

const THEMES: readonly Choice<ThemePreference>[] = [
  { value: "system", label: "◐ System", description: "Follow this device's setting." },
  { value: "light", label: "☀ Light" },
  { value: "dark", label: "☾ Dark" },
];

const DETAILS: readonly Choice<DetailsMode>[] = [
  { value: "panel", label: "Panel under the list", description: "Click a torrent to show its details below the list. Double-click still opens a window." },
  { value: "window", label: "Separate windows", description: "Click only selects. Double-click opens the torrent in a window of its own." },
];

export function AppearanceSection() {
  const theme = useTheme();
  const details = useDetailsMode();
  return (
    <>
      <SettingCard title="Theme" description="Only for this browser or app; applies at once.">
        <ChoiceCards label="Theme" columns={3} value={theme} options={THEMES.filter((t) => THEME_PREFERENCES.includes(t.value))} onChange={setTheme} />
      </SettingCard>
      <SettingCard
        title="Torrent details"
        description="Where a torrent's details show on a wide screen. Torrent windows stay open after the list's window closes. Only for this browser or app."
      >
        <ChoiceCards label="Torrent details" columns={2} value={details} options={DETAILS} onChange={setDetailsMode} />
      </SettingCard>
    </>
  );
}
