// The app: the fleet controller in context, the routes (DESIGN.md §2.3),
// and the pages' shared chrome.

import {
  createRootRoute,
  createRoute,
  createRouter,
  Navigate,
  RouterProvider,
} from "@tanstack/react-router";
import { type ReactElement, useEffect, useState } from "react";

import { Bridge } from "./bridge/client";
import { forgetToken, type Credential } from "./bridge/token";
import { FleetContext } from "./context";
import { Fleet } from "./data/fleet";
import { DevicesPage } from "./routes/devices";
import { FleetPage } from "./routes/fleet";
import { SessionPage } from "./routes/session";
import { SessionsPage } from "./routes/sessions";
import { SettingsPage } from "./routes/settings";
import { SyncPage } from "./routes/sync";
import { Shell } from "./ui/shell";

/** `?node=` on the pages scoped by node. */
function nodeSearch(search: Record<string, unknown>): { node?: string } {
  return typeof search.node === "string" ? { node: search.node } : {};
}

/** `/` is Fleet: there is no home dashboard apart from it (DESIGN.md §2.1).
 * (A component, not a `beforeLoad` redirect: the redirect's typed target
 * would make the route tree's type refer to itself.) */
function Home(): ReactElement {
  return <Navigate to="/fleet" replace />;
}

const rootRoute = createRootRoute({ component: Shell });

const routeTree = rootRoute.addChildren([
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/",
    component: Home,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/fleet",
    component: FleetPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/fleet/$node",
    component: FleetPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/sessions",
    validateSearch: nodeSearch,
    component: SessionsPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/sessions/$node/$id",
    component: SessionPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/sync",
    validateSearch: nodeSearch,
    component: SyncPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/devices",
    validateSearch: nodeSearch,
    component: DevicesPage,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/settings",
    component: SettingsPage,
  }),
]);

export const router = createRouter({ routeTree, defaultPreload: false });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}

/** Shown when the page has no token, or the server refused it. */
function NoToken({ refused }: { refused: boolean }) {
  return (
    <div className="empty" role="main">
      <h2>
        {refused
          ? "The UI was restarted or its identity could not be verified"
          : "Open Vessel from its address"}
      </h2>
      <p>
        Run <code>vessel ui</code> and open the address it prints. Each launch
        has a one-time address; keep this tab open after using it.
      </p>
    </div>
  );
}

export function App({ token }: { token: Credential | null }) {
  const [fleet] = useState(() =>
    token === null ? null : new Fleet(new Bridge(token)),
  );
  const [refused, setRefused] = useState(false);
  useEffect(() => {
    if (fleet === null) {
      return undefined;
    }
    const off = fleet.bridge.onLink((link) => {
      if (link.kind === "refused") {
        forgetToken();
        setRefused(true);
      }
    });
    fleet.start();
    return () => {
      off();
      fleet.stop();
    };
  }, [fleet]);
  if (fleet === null || refused) {
    return <NoToken refused={refused} />;
  }
  return (
    <FleetContext.Provider value={fleet}>
      <RouterProvider router={router} />
    </FleetContext.Provider>
  );
}
