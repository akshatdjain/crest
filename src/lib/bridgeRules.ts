import type { BridgeAction } from "./bridge";

/**
 * What the notch does when the settings window closes: a shortcut being recorded there paused Crest's shortcuts, and the
 * window may close before the recording ends.
 */
export function settingsClosedActions(): BridgeAction[] {
  return [{ kind: "pauseHotkeys", on: false }];
}
