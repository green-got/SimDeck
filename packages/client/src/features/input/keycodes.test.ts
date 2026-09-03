import { describe, expect, it } from "vitest";

import { keyboardModifiers, keyCodeForKeyboardEvent } from "./keycodes";

function keyboardEventLike(overrides: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    code: "",
    key: "",
    ...overrides,
  } as KeyboardEvent;
}

describe("keyCodeForKeyboardEvent", () => {
  it("maps a logical letter to the French AZERTY simulator layout", () => {
    const qwerty = keyboardEventLike({ code: "KeyA", key: "a" });
    const azerty = keyboardEventLike({ code: "KeyQ", key: "a" });

    expect(keyCodeForKeyboardEvent(qwerty)).toBe(20);
    expect(keyCodeForKeyboardEvent(azerty)).toBe(20);
  });

  it("maps shifted printable characters to their underlying key", () => {
    const event = keyboardEventLike({
      code: "Slash",
      key: "?",
    });

    expect(keyCodeForKeyboardEvent(event)).toBe(56);
  });

  it("falls back to the physical code for control keys", () => {
    const event = keyboardEventLike({
      code: "ArrowLeft",
      key: "ArrowLeft",
    });

    expect(keyCodeForKeyboardEvent(event)).toBe(80);
  });

  it("maps escape to the USB HID escape usage instead of macOS virtual keycode", () => {
    const event = keyboardEventLike({
      code: "Escape",
      key: "Escape",
    });

    expect(keyCodeForKeyboardEvent(event)).toBe(41);
  });

  it("maps h to the USB HID h usage instead of macOS virtual keycode", () => {
    const event = keyboardEventLike({
      code: "KeyH",
      key: "h",
    });

    expect(keyCodeForKeyboardEvent(event)).toBe(11);
  });

  it("keeps Caps Lock local to the browser", () => {
    const event = keyboardEventLike({
      code: "CapsLock",
      getModifierState: (modifier) => modifier === "CapsLock",
      key: "CapsLock",
    });

    expect(keyCodeForKeyboardEvent(event)).toBeNull();
    expect(keyboardModifiers(event)).toBe(0);
  });
});
