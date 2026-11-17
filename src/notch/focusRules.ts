/** This many quick clicks on the notch toggle focus mode (EXIT_CLICKS in hit_test.rs counts the ones that leave it). */
export const FOCUS_CLICKS = 3;

/**
 * Whether a click toggles focus mode: only with its switch on (display.focusClicks), from the third quick click on, and never on a
 * control or a text field, where a triple click selects or presses something.
 */
export function togglesFocus(clicks: number, enabled: boolean, interactive: boolean): boolean {
  return enabled && clicks >= FOCUS_CLICKS && !interactive;
}
