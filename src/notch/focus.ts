import { createStore } from "../lib/store";
import { onFocusExit } from "../platform/native";

/*
 * Focus mode: the notch stays visible but fully click-through, so clicks reach the app below (e.g. browser tabs at the very top).
 * Look per setting: half transparent, or shrunk to a thin line at the edge (display.focusStyle). In and out: three quick clicks on
 * the notch. Natively Rust counts the clicks that leave it, because the window no longer gets any (except under a card with
 * buttons, Notch.tsx). Not saved: Crest always starts out of it, so a notch put aside by accident is back after a restart.
 */
export const focusMode = createStore(false);

/** This many quick clicks toggle it (like EXIT_CLICKS in hit_test.rs). */
export const FOCUS_CLICKS = 3;

export function setFocusMode(on: boolean) {
  focusMode.set(on);
}

let started = false;

export function startFocusMode() {
  if (started) return;
  started = true;
  // Notch.tsx sets the click-through; the clicks that leave focus mode come from Rust.
  onFocusExit(() => setFocusMode(false));
}
