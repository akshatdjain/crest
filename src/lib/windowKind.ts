/*
 * Which of Crest's windows this page runs in, from its page: the notch (index.html, and anything else), the detached timer, a copy of
 * the closed notch on another monitor, or the settings window. The notch alone applies settings to Rust, plays sounds and owns the
 * in-memory state; the other windows read and ask (src/lib/bridge.ts).
 */

export type WindowKind = "notch" | "timer" | "mirror" | "settings";

export function kindOf(pathname: string): WindowKind {
  if (pathname.endsWith("/timer.html")) return "timer";
  if (pathname.endsWith("/mirror.html")) return "mirror";
  if (pathname.endsWith("/settings.html")) return "settings";
  return "notch";
}

export const windowKind: WindowKind = typeof location === "undefined" ? "notch" : kindOf(location.pathname);
export const isNotchWindow = windowKind === "notch";
export const isTimerWindow = windowKind === "timer";
