import { test } from "node:test";
import assert from "node:assert/strict";
import { followStores, onActions, sendAction, shareStores, type Transport } from "./bridge.ts";
import { settingsClosedActions } from "./bridgeRules.ts";

/** Both windows on one bus: an emit reaches every handler of its event at once, whatever window it names. */
function memoryBus() {
  const handlers = new Map<string, Set<(payload: unknown) => void>>();
  const counts = new Map<string, number>();
  const bus: Transport & { sent(event: string): number } = {
    emit(event, payload) {
      counts.set(event, (counts.get(event) ?? 0) + 1);
      [...(handlers.get(event) ?? [])].forEach((h) => h(payload));
    },
    listen(event, handler) {
      if (!handlers.has(event)) handlers.set(event, new Set());
      handlers.get(event)!.add(handler);
      return () => void handlers.get(event)!.delete(handler);
    },
    sent: (event) => counts.get(event) ?? 0,
  };
  return bus;
}

function memStore<T>(initial: T) {
  let value = initial;
  const listeners = new Set<() => void>();
  return {
    get: () => value,
    set(next: unknown) {
      value = next as T;
      listeners.forEach((l) => l());
    },
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => void listeners.delete(listener);
    },
  };
}

test("the settings window gets every shared value after hello, then each change", () => {
  const bus = memoryBus();
  const notchSide = { a: memStore(1), b: memStore("x") }, settingsSide = { a: memStore(0), b: memStore("") };
  shareStores(bus, notchSide);
  followStores(bus, settingsSide);
  assert.equal(settingsSide.a.get(), 1);
  assert.equal(settingsSide.b.get(), "x");
  notchSide.a.set(2);
  assert.equal(settingsSide.a.get(), 2);
});

test("nothing is sent before a hello, and nothing after stop", () => {
  const bus = memoryBus();
  const notchSide = { a: memStore(1) };
  const stop = shareStores(bus, notchSide);
  notchSide.a.set(2);
  assert.equal(bus.sent("bridge://state"), 0);
  stop();
  bus.emit("bridge://hello", null);
  notchSide.a.set(3);
  assert.equal(bus.sent("bridge://state"), 0);
});

test("unknown store names are ignored", () => {
  const bus = memoryBus();
  const settingsSide = { a: memStore(0) };
  followStores(bus, settingsSide);
  bus.emit("bridge://state", { name: "secret", value: 9 });
  bus.emit("bridge://state", { name: "constructor", value: 9 });
  assert.equal(settingsSide.a.get(), 0);
});

test("only the known actions reach the notch", () => {
  const bus = memoryBus();
  const ran: unknown[] = [];
  onActions(bus, (a) => ran.push(a));
  sendAction(bus, { kind: "refresh", what: "github", force: true });
  bus.emit("bridge://action", { kind: "deleteEverything" });
  bus.emit("bridge://action", { kind: "refresh", what: "vault" });
  assert.deepEqual(ran, [{ kind: "refresh", what: "github", force: true }]);
});

test("closing_the_settings_window_resumes_hotkeys", () => {
  assert.deepEqual(settingsClosedActions(), [{ kind: "pauseHotkeys", on: false }]);
});
