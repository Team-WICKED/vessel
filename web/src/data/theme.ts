// Dark is the design; light is a re-derived theme (DESIGN.md §4). The
// operator's choice is kept in `localStorage`; until there is one, the
// system's preference decides. Applied before the first paint, so the
// page never flashes the other theme.

export type Theme = "dark" | "light";

const KEY = "vessel.theme";

export function storedTheme(): Theme | null {
  const stored = window.localStorage.getItem(KEY);
  return stored === "dark" || stored === "light" ? stored : null;
}

export function currentTheme(): Theme {
  return (
    storedTheme() ??
    (window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark")
  );
}

export function applyTheme(theme: Theme = currentTheme()): void {
  document.documentElement.dataset.theme = theme;
}

export function chooseTheme(theme: Theme): void {
  window.localStorage.setItem(KEY, theme);
  applyTheme(theme);
}
