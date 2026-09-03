import { useCallback, useEffect, useRef } from "react";

import { isEditableTarget } from "./keycodes";
import {
  isPrintableKeyboardEvent,
  SemanticKeyboardBatcher,
  SemanticKeyboardTranslator,
} from "./semanticKeyboard";

interface UseKeyboardInputOptions {
  enabled: boolean;
  onKey: (payload: { keyCode: number; modifiers: number }) => void;
  onShortcut: (payload: {
    key: string;
    keyCode: number;
    modifiers: number;
  }) => void;
  onText: (text: string) => void;
  onToggleSoftwareKeyboard?: () => void;
}

export function useKeyboardInput({
  enabled,
  onKey,
  onShortcut,
  onText,
  onToggleSoftwareKeyboard,
}: UseKeyboardInputOptions) {
  const sinkRef = useRef<HTMLTextAreaElement | null>(null);
  const onKeyRef = useRef(onKey);
  const onShortcutRef = useRef(onShortcut);
  const onTextRef = useRef(onText);
  const onToggleSoftwareKeyboardRef = useRef(onToggleSoftwareKeyboard);

  useEffect(() => {
    onKeyRef.current = onKey;
  }, [onKey]);

  useEffect(() => {
    onShortcutRef.current = onShortcut;
  }, [onShortcut]);

  useEffect(() => {
    onTextRef.current = onText;
  }, [onText]);

  useEffect(() => {
    onToggleSoftwareKeyboardRef.current = onToggleSoftwareKeyboard;
  }, [onToggleSoftwareKeyboard]);

  useEffect(() => {
    if (!enabled) {
      return;
    }

    const sink = sinkRef.current;
    if (!(sink instanceof HTMLTextAreaElement)) {
      return;
    }
    const keyboardSink = sink;
    const batcher = new SemanticKeyboardBatcher({
      onKey: (payload) => onKeyRef.current(payload),
      onShortcut: (payload) => onShortcutRef.current(payload),
      onText: (text) => onTextRef.current(text),
    });
    const translator = new SemanticKeyboardTranslator(batcher);
    let compositionCommitTimer: ReturnType<typeof setTimeout> | null = null;
    let suppressInputTimer: ReturnType<typeof setTimeout> | null = null;
    let suppressNextInput = false;

    function handleWindowKeyDown(event: KeyboardEvent) {
      if (isEditableTarget(event.target) && event.target !== keyboardSink) {
        return;
      }
      if (isCopyShortcut(event) && hasDocumentSelection()) {
        return;
      }
      if (isSoftwareKeyboardShortcut(event)) {
        event.preventDefault();
        event.stopImmediatePropagation();
        onToggleSoftwareKeyboardRef.current?.();
        return;
      }
      if (
        isPrintableKeyboardEvent(event) &&
        document.activeElement !== keyboardSink
      ) {
        keyboardSink.focus({ preventScroll: true });
      }
      if (translator.keyDown(event)) {
        event.preventDefault();
      }
    }

    function handleBeforeInput(event: InputEvent) {
      if (
        translator.beforeInput(event.inputType, event.data, event.isComposing)
      ) {
        suppressNextInput = true;
        if (suppressInputTimer) {
          clearTimeout(suppressInputTimer);
        }
        suppressInputTimer = setTimeout(() => {
          suppressNextInput = false;
          suppressInputTimer = null;
        }, 0);
        event.preventDefault();
      }
    }

    function handleInput(event: Event) {
      const inputEvent = event as InputEvent;
      if (!inputEvent.isComposing && keyboardSink.value && !suppressNextInput) {
        batcher.text(keyboardSink.value);
      }
      suppressNextInput = false;
      if (suppressInputTimer) {
        clearTimeout(suppressInputTimer);
        suppressInputTimer = null;
      }
      keyboardSink.value = "";
    }

    function handlePaste(event: ClipboardEvent) {
      if (translator.paste(event.clipboardData?.getData("text/plain") ?? "")) {
        event.preventDefault();
      }
    }

    function handleCompositionStart() {
      translator.compositionStart();
    }

    function handleCompositionEnd(event: CompositionEvent) {
      translator.compositionEnd(event.data);
      keyboardSink.value = "";
      if (compositionCommitTimer) {
        clearTimeout(compositionCommitTimer);
      }
      compositionCommitTimer = setTimeout(() => {
        translator.clearCompositionCommit();
        compositionCommitTimer = null;
      }, 0);
    }

    window.addEventListener("keydown", handleWindowKeyDown);
    keyboardSink.addEventListener("beforeinput", handleBeforeInput);
    keyboardSink.addEventListener("compositionend", handleCompositionEnd);
    keyboardSink.addEventListener("compositionstart", handleCompositionStart);
    keyboardSink.addEventListener("input", handleInput);
    keyboardSink.addEventListener("paste", handlePaste);
    return () => {
      batcher.flush();
      if (compositionCommitTimer) {
        clearTimeout(compositionCommitTimer);
      }
      if (suppressInputTimer) {
        clearTimeout(suppressInputTimer);
      }
      window.removeEventListener("keydown", handleWindowKeyDown);
      keyboardSink.removeEventListener("beforeinput", handleBeforeInput);
      keyboardSink.removeEventListener("compositionend", handleCompositionEnd);
      keyboardSink.removeEventListener(
        "compositionstart",
        handleCompositionStart,
      );
      keyboardSink.removeEventListener("input", handleInput);
      keyboardSink.removeEventListener("paste", handlePaste);
    };
  }, [enabled]);

  const focus = useCallback(() => {
    sinkRef.current?.focus({ preventScroll: true });
  }, []);

  return { focus, sinkRef };
}

function isSoftwareKeyboardShortcut(event: KeyboardEvent): boolean {
  return (
    event.key.toLowerCase() === "k" &&
    event.metaKey &&
    !event.ctrlKey &&
    !event.altKey &&
    !event.shiftKey
  );
}

function isCopyShortcut(event: KeyboardEvent): boolean {
  return (
    event.key.toLowerCase() === "c" &&
    !event.altKey &&
    !event.shiftKey &&
    (event.metaKey || event.ctrlKey)
  );
}

function hasDocumentSelection(): boolean {
  const selection = window.getSelection();
  return Boolean(selection && !selection.isCollapsed && selection.toString());
}
