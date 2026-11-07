// The web UI's entry: the launch token, the theme before first paint,
// then the app (ADR 0017).
import "./ui/fonts.css";
import "./ui/tokens.css";
import "./ui/app.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./app";
import { takeToken } from "./bridge/token";
import { applyTheme } from "./data/theme";

applyTheme();
const token = takeToken();
const root = document.getElementById("root");
if (root !== null) {
  createRoot(root).render(
    <StrictMode>
      <App token={token} />
    </StrictMode>,
  );
}
