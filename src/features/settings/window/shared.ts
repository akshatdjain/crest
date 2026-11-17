import type { RestoreResult } from "../../../lib/backup";
import { shareStores, type Shared } from "../../../lib/bridge";
import { restoreResult } from "../../../lib/restorePending";
import { createStore } from "../../../lib/store";
import { tauriTransport } from "../../../lib/tauriTransport";
import { update } from "../../../lib/update";
import { isNotchWindow } from "../../../lib/windowKind";
import { liveSupport } from "../../../notch/glass/live";
import { monitor, monitors } from "../../../notch/monitor";
import { isNative } from "../../../platform/native";
import { calendar } from "../../calendar/store";
import { discordState } from "../../discord/store";
import { github } from "../../github/store";
import { hotkeysResting, shortcutResult } from "../../launcher/store";
import { leetcode } from "../../leetcode/store";
import { plugins } from "../../plugins/store";
import { privacy, snoozed } from "../../privacy/store";
import { system } from "../../system/store";
import { hotkeyStatus, toolHotkeyStatus } from "../../window/store";

/*
 * What the settings window shows from the notch's memory. Only the notch's start functions fill these stores (update checks,
 * shortcut registration, readings, the monitor the notch is on), so the settings window takes them over the bridge
 * (src/lib/bridge.ts). Without them it would show Discord as off, hide the glass switches and size the closed notch on its own
 * screen.
 */

/** The restore applied when the notch started (BackupSettings shows it); the settings window gets it over the bridge. */
export const restored = createStore<RestoreResult>(restoreResult);

export const sharedStores: Record<string, Shared> = {
  update,
  hotkeyStatus,
  toolHotkeyStatus,
  shortcutResult,
  hotkeysResting,
  privacy,
  snoozed,
  github,
  leetcode,
  calendar,
  system,
  plugins,
  discordState,
  liveSupport,
  monitor,
  monitors,
  restored,
};

/**
 * Notch side: shares the stores with the settings window, in the app only. It waits for the window's hello from the start (the
 * page loads while Rust is still placing the window, so a hello can come before "settings-window://open"), and a closed window
 * ends the sharing, so no change is sent while none is open.
 */
export function startSettingsBridge() {
  if (!isNative || !isNotchWindow) return;
  let stop = shareStores(tauriTransport, sharedStores);
  tauriTransport.listen("settings-window://open", (open) => {
    if (open !== false) return;
    stop();
    stop = shareStores(tauriTransport, sharedStores);
  });
}
