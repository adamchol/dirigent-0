// Dark (the default), light, or the system's: the `dark` class on <html> (index.css's dark
// variant), kept in localStorage. index.html sets it before the page renders, so it doesn't flash.

import { createContext, useContext, useEffect, useState } from "react";

export type Theme = "light" | "dark" | "system";

const KEY = "dirigent.theme";

const ThemeContext = createContext<{ theme: Theme; setTheme: (t: Theme) => void }>({
  theme: "dark",
  setTheme: () => {},
});

function stored(): Theme {
  try {
    const t = localStorage.getItem(KEY);
    return t === "light" || t === "system" ? t : "dark";
  } catch {
    return "dark";
  }
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [theme, setTheme] = useState<Theme>(stored);
  useEffect(() => {
    const dark = matchMedia("(prefers-color-scheme: dark)");
    const apply = () =>
      document.documentElement.classList.toggle("dark", theme === "dark" || (theme === "system" && dark.matches));
    apply();
    try {
      localStorage.setItem(KEY, theme);
    } catch {
      // a private window: not remembered
    }
    dark.addEventListener("change", apply);
    return () => dark.removeEventListener("change", apply);
  }, [theme]);
  return <ThemeContext value={{ theme, setTheme }}>{children}</ThemeContext>;
}

export function useTheme() {
  return useContext(ThemeContext);
}
