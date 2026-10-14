import { useCallback, type MouseEventHandler } from "react";
import { startWindowDrag } from "../lib/transport";

/**
 * Returns a mousedown handler that initiates window dragging via Tauri API.
 * No-op in web mode, where there is no OS window to drag.
 */
export function useDragWindow(): MouseEventHandler {
  return useCallback((e) => {
    if (e.buttons === 1 && e.detail === 1) {
      startWindowDrag();
    }
  }, []);
}
