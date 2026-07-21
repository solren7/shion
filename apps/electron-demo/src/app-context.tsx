import { createContext, useContext } from "react";

import type { KomoConnectResponse } from "./global";

export type View = "chat" | "dashboard";

/** Gateway connection status, refreshed on a timer by `App`. */
export const ConnectionContext = createContext<KomoConnectResponse>({ connected: false });
export const useConnection = () => useContext(ConnectionContext);

/** Cross-view navigation + the active chat session (shared so the dashboard's
 *  Sessions tab can open a past session in the chat view). */
export interface Nav {
  view: View;
  setView: (v: View) => void;
  session: string;
  setSession: (s: string) => void;
}

export const NavContext = createContext<Nav>({
  view: "chat",
  setView: () => {},
  session: "",
  setSession: () => {},
});
export const useNav = () => useContext(NavContext);
