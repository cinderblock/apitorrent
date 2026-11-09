import { THEME_PREFERENCES, type ThemePreference, setTheme, useTheme } from "../theme";

const LABELS: Record<ThemePreference, string> = {
  system: "◐ System",
  light: "☀ Light",
  dark: "☾ Dark",
};

/** This device's colour theme. */
export function ThemePicker() {
  const theme = useTheme();
  return (
    <select
      className="theme-picker"
      aria-label="Colour theme"
      title="Colour theme on this device"
      value={theme}
      onChange={(e) => setTheme(e.target.value as ThemePreference)}
    >
      {THEME_PREFERENCES.map((t) => (
        <option key={t} value={t}>
          {LABELS[t]}
        </option>
      ))}
    </select>
  );
}
