// The app's entry: the page, rendered into #root (kept across hot reloads in development).

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App.tsx";

const app = (
  <StrictMode>
    <App />
  </StrictMode>
);

if (import.meta.hot) (import.meta.hot.data.root ??= createRoot(document.getElementById("root")!)).render(app);
else createRoot(document.getElementById("root")!).render(app);
