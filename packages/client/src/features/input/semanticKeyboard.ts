import { keyCodeForKeyboardEvent, keyboardModifiers } from "./keycodes";

export interface KeyboardEventLike {
  altKey: boolean;
  code: string;
  ctrlKey: boolean;
  getModifierState?: (key: string) => boolean;
  isComposing?: boolean;
  key: string;
  metaKey: boolean;
  shiftKey: boolean;
}

interface SemanticKeyboardBatcherOptions {
  delayMs?: number;
  onKey: (payload: { keyCode: number; modifiers: number }) => void;
  onShortcut: (payload: {
    key: string;
    keyCode: number;
    modifiers: number;
  }) => void;
  onText: (text: string) => void;
}

const TEXT_INPUT_TYPES = new Set([
  "insertText",
  "insertReplacementText",
  "insertTranspose",
]);

export function isPrintableKeyboardEvent(event: KeyboardEventLike): boolean {
  return (
    event.isComposing === true ||
    event.key === "Dead" ||
    event.key === "Process" ||
    event.key.length === 1
  );
}

export function isPhysicalShortcut(event: KeyboardEventLike): boolean {
  return event.metaKey || (event.ctrlKey && !event.altKey);
}

export class SemanticKeyboardBatcher {
  private pendingText = "";
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(private readonly options: SemanticKeyboardBatcherOptions) {}

  text(value: string): void {
    this.pendingText += value;
    if (this.timer) {
      return;
    }
    this.timer = setTimeout(() => this.flush(), this.options.delayMs ?? 32);
  }

  key(payload: { keyCode: number; modifiers: number }): void {
    this.flush();
    this.options.onKey(payload);
  }

  shortcut(payload: { key: string; keyCode: number; modifiers: number }): void {
    this.flush();
    this.options.onShortcut(payload);
  }

  flush(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    const text = this.pendingText;
    this.pendingText = "";
    if (text) {
      this.options.onText(text);
    }
  }
}

export class SemanticKeyboardTranslator {
  private composing = false;
  private ignoredCompositionCommit: string | null = null;

  constructor(private readonly batcher: SemanticKeyboardBatcher) {}

  keyDown(event: KeyboardEventLike): boolean {
    if (isPasteShortcut(event)) {
      return false;
    }
    if (isPrintableKeyboardEvent(event) && !isPhysicalShortcut(event)) {
      return false;
    }

    const keyCode = keyCodeForKeyboardEvent(event as KeyboardEvent);
    if (keyCode == null) {
      return false;
    }
    const modifiers = keyboardModifiers(event as KeyboardEvent);
    if (event.key.length === 1 && isPhysicalShortcut(event)) {
      const key = /^[A-Z]$/.test(event.key)
        ? event.key.toLowerCase()
        : event.key;
      this.batcher.shortcut({ key, keyCode, modifiers });
    } else {
      this.batcher.key({ keyCode, modifiers });
    }
    return true;
  }

  beforeInput(
    inputType: string,
    data: string | null,
    isComposing = false,
  ): boolean {
    if (isComposing || this.composing || inputType.includes("Composition")) {
      return false;
    }
    if (inputType === "insertFromPaste" || inputType === "insertFromDrop") {
      return true;
    }
    if (!TEXT_INPUT_TYPES.has(inputType) || !data) {
      return false;
    }
    if (this.ignoredCompositionCommit === data) {
      this.ignoredCompositionCommit = null;
      return true;
    }
    this.ignoredCompositionCommit = null;
    this.batcher.text(data);
    return true;
  }

  paste(text: string): boolean {
    if (!text) {
      return false;
    }
    this.batcher.text(text);
    return true;
  }

  compositionStart(): void {
    this.composing = true;
    this.ignoredCompositionCommit = null;
  }

  compositionEnd(text: string): boolean {
    this.composing = false;
    if (!text) {
      return false;
    }
    this.batcher.text(text);
    this.ignoredCompositionCommit = text;
    return true;
  }

  clearCompositionCommit(): void {
    this.ignoredCompositionCommit = null;
  }
}

function isPasteShortcut(event: KeyboardEventLike): boolean {
  return event.code === "KeyV" && isPhysicalShortcut(event);
}
