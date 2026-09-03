import { afterEach, describe, expect, it, vi } from "vitest";

import {
  SemanticKeyboardBatcher,
  SemanticKeyboardTranslator,
  type KeyboardEventLike,
} from "./semanticKeyboard";

function keyboardEvent(
  overrides: Partial<KeyboardEventLike>,
): KeyboardEventLike {
  return {
    altKey: false,
    code: "",
    ctrlKey: false,
    key: "",
    metaKey: false,
    shiftKey: false,
    ...overrides,
  };
}

function keyboardHarness() {
  const keys: Array<{ keyCode: number; modifiers: number }> = [];
  const text: string[] = [];
  const batcher = new SemanticKeyboardBatcher({
    delayMs: 16,
    onKey: (payload) => keys.push(payload),
    onText: (value) => text.push(value),
  });
  return {
    batcher,
    keys,
    text,
    translator: new SemanticKeyboardTranslator(batcher),
  };
}

afterEach(() => vi.useRealTimers());

describe("SemanticKeyboardTranslator", () => {
  it("lets the browser resolve printable keys instead of sending physical HID", () => {
    const { keys, translator } = keyboardHarness();

    expect(translator.keyDown(keyboardEvent({ code: "KeyQ", key: "a" }))).toBe(
      false,
    );
    expect(keys).toEqual([]);
  });

  it("treats AltGr and Option output as semantic text", () => {
    const { keys, translator } = keyboardHarness();

    expect(
      translator.keyDown(
        keyboardEvent({
          altKey: true,
          code: "Digit0",
          ctrlKey: true,
          key: "@",
        }),
      ),
    ).toBe(false);
    expect(keys).toEqual([]);
  });

  it("flushes semantic text before navigation HID", () => {
    const operations: string[] = [];
    const batcher = new SemanticKeyboardBatcher({
      delayMs: 16,
      onKey: ({ keyCode }) => operations.push(`key:${keyCode}`),
      onText: (value) => operations.push(`text:${value}`),
    });
    const translator = new SemanticKeyboardTranslator(batcher);
    translator.beforeInput("insertText", "é");

    expect(
      translator.keyDown(
        keyboardEvent({ code: "ArrowLeft", key: "ArrowLeft" }),
      ),
    ).toBe(true);
    expect(operations).toEqual(["text:é", "key:80"]);
  });

  it("types human-speed browser text as exact semantic HID keys", () => {
    vi.useFakeTimers();
    const { keys, text, translator } = keyboardHarness();

    for (const character of "schaaf") {
      translator.beforeInput("insertText", character);
      vi.advanceTimersByTime(120);
    }

    expect(keys).toEqual(
      [22, 6, 11, 20, 20, 9].map((keyCode) => ({ keyCode, modifiers: 0 })),
    );
    expect(text).toEqual([]);
  });

  it("derives letters independently of the browser keyboard layout", () => {
    const { keys, translator } = keyboardHarness();

    expect(translator.keyDown(keyboardEvent({ code: "KeyA", key: "q" }))).toBe(
      false,
    );
    expect(translator.beforeInput("insertText", "q")).toBe(true);
    expect(translator.keyDown(keyboardEvent({ code: "KeyQ", key: "q" }))).toBe(
      false,
    );
    expect(translator.beforeInput("insertText", "q")).toBe(true);

    expect(keys).toEqual([
      { keyCode: 4, modifiers: 0 },
      { keyCode: 4, modifiers: 0 },
    ]);
  });

  it("uses semantic text for symbols whose modifiers vary by layout", () => {
    vi.useFakeTimers();
    const { keys, text, translator } = keyboardHarness();

    expect(translator.beforeInput("insertText", "@")).toBe(true);
    vi.runAllTimers();

    expect(keys).toEqual([]);
    expect(text).toEqual(["@"]);
  });

  it("uses browser-resolved casing without toggling remote Caps Lock", () => {
    const { keys, translator } = keyboardHarness();

    expect(
      translator.keyDown(
        keyboardEvent({
          code: "CapsLock",
          getModifierState: (modifier) => modifier === "CapsLock",
          key: "CapsLock",
        }),
      ),
    ).toBe(false);
    expect(translator.beforeInput("insertText", "A")).toBe(true);
    expect(translator.beforeInput("insertText", "a")).toBe(true);

    expect(keys).toEqual([
      { keyCode: 20, modifiers: 1 },
      { keyCode: 20, modifiers: 0 },
    ]);
  });

  it("commits composed Unicode once", () => {
    vi.useFakeTimers();
    const { text, translator } = keyboardHarness();
    translator.compositionStart();

    expect(translator.beforeInput("insertCompositionText", "é", true)).toBe(
      false,
    );
    expect(translator.compositionEnd("é")).toBe(true);
    expect(translator.beforeInput("insertText", "é")).toBe(true);
    vi.runAllTimers();

    expect(text).toEqual(["é"]);
  });

  it("uses semantic text for paste rather than sending Command-V HID", () => {
    vi.useFakeTimers();
    const { keys, text, translator } = keyboardHarness();

    expect(
      translator.keyDown(
        keyboardEvent({ code: "KeyV", key: "v", metaKey: true }),
      ),
    ).toBe(false);
    expect(translator.paste("élise@example.com")).toBe(true);
    vi.runAllTimers();

    expect(keys).toEqual([]);
    expect(text).toEqual(["élise@example.com"]);
  });
});
