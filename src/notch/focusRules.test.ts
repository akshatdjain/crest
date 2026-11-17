import { test } from "node:test";
import assert from "node:assert/strict";
import { togglesFocus } from "./focusRules.ts";

test("with the switch off no clicks enter focus mode", () => {
  for (const n of [1, 2, 3, 4, 7]) assert.equal(togglesFocus(n, false, false), false);
});

test("with the switch on three quick clicks toggle it, not fewer", () => {
  assert.equal(togglesFocus(2, true, false), false);
  assert.equal(togglesFocus(3, true, false), true);
  assert.equal(togglesFocus(4, true, false), true);
});

test("a triple click on a control or a text field never toggles it", () => {
  assert.equal(togglesFocus(3, true, true), false);
});
