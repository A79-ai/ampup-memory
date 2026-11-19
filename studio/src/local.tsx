// The pages as `ampup-memory serve --local` serves them at /studio: the memory of the server they come from,
// read as its one caller. No sign-in and no org: the server serves one memory, on this machine only.
import { StrictMode, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource/atkinson-hyperlegible-next/400.css";
import "@fontsource/atkinson-hyperlegible-next/600.css";
import "@fontsource/atkinson-hyperlegible-next/700.css";
import "@fontsource/atkinson-hyperlegible-mono/400.css";
import { LocalTransport, Logo, MemoryStudio } from "./index";
import "./local.css";

type Theme = "light" | "dark" | "system";
const THEMES: [Theme, string][] = [
  ["system", "System"],
  ["light", "Light"],
  ["dark", "Dark"],
];
const KEY = "ampup-memory-studio:theme";

function readTheme(): Theme {
  try {
    const t = localStorage.getItem(KEY);
    if (t === "light" || t === "dark" || t === "system") return t;
  } catch {
    // Storage refused (a private window): the system's.
  }
  return "system";
}

function Local() {
  const transport = useMemo(() => new LocalTransport(), []);
  const [theme, setTheme] = useState<Theme>(readTheme);
  useEffect(() => {
    // The page around the pages follows too (its background, the scrollbars).
    document.documentElement.dataset.studioTheme = theme;
    try {
      localStorage.setItem(KEY, theme);
    } catch {
      // Not kept: it still applies to this page.
    }
  }, [theme]);
  return (
    <div className="local-shell">
      <header className="local-top">
        <a className="local-brand" href="#/home">
          <Logo size={22} />
          <span>AmpUp Memory</span>
          <span className="local-where">this machine</span>
        </a>
        <label className="local-theme">
          <span>Theme</span>
          <select value={theme} onChange={(e) => setTheme(e.target.value as Theme)}>
            {THEMES.map(([t, label]) => (
              <option key={t} value={t}>
                {label}
              </option>
            ))}
          </select>
        </label>
      </header>
      <main className="local-main">
        <MemoryStudio transport={transport} org="Local memory" theme={theme} />
      </main>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <Local />
  </StrictMode>,
);
