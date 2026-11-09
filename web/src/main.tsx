import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { AuthGate } from "./auth/AuthGate";
import { DesktopGate } from "./desktop/DesktopGate";
import { installDesktop } from "./desktop/install";
import "./styles.css";

const desktop = installDesktop();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    {desktop ? (
      <DesktopGate>
        <AuthGate />
      </DesktopGate>
    ) : (
      <AuthGate />
    )}
  </StrictMode>,
);
