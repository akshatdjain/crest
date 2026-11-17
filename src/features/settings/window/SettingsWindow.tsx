import { getCurrentWindow } from "@tauri-apps/api/window";
import { X } from "@phosphor-icons/react";
import { t } from "../../../i18n";
import { HeaderButton } from "../../../notch/header";
import { isNative } from "../../../platform/native";
import { SettingsView } from "../SettingsView";

/**
 * The settings window: a title row to drag the window by and to close it, then the settings as the notch shows them.
 * No data-tauri-drag-region: its double click asks for a maximize the window cannot do.
 */
export function SettingsWindow() {
  return (
    <div className="flex h-full flex-col bg-notch">
      <div
        className="flex h-11 shrink-0 items-center justify-between ps-4 pe-2"
        onMouseDown={(e) => {
          if (e.button === 0 && isNative && !(e.target as Element).closest("button")) void getCurrentWindow().startDragging();
        }}
      >
        <h1 className="text-[13px] font-semibold text-label-2">{t("Einstellungen")}</h1>
        <HeaderButton label={t("Schließen")} onClick={() => isNative && void getCurrentWindow().close()}>
          <X size={14} weight="bold" />
        </HeaderButton>
      </div>
      <div className="min-h-0 flex-1">
        <SettingsView />
      </div>
    </div>
  );
}
