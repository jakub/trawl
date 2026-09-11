// CodeMirror 6 wrapper consumed by the Leptos SPA via wasm-bindgen.
//
// Kept deliberately thin: the wasm side passes in callbacks and an
// initial document, gets back a handle with a couple of methods. All
// DSL-specific behavior (lint, autocomplete) is driven by the callbacks
// rather than baked into this bundle so the trawl-core parser stays the
// single source of truth.

import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { linter, lintGutter, Diagnostic } from "@codemirror/lint";
import {
  autocompletion,
  CompletionContext,
  CompletionResult,
} from "@codemirror/autocomplete";

export interface EditorOpts {
  /** Called on every document change with the current doc text. */
  onChange: (doc: string) => void;
  /** Called when the user hits cmd/ctrl+Enter. */
  onSubmit: () => void;
  /** Synchronous linter — wasm side runs trawl-core::parser::parse. */
  lint: (doc: string) => Diagnostic[];
  /** Autocomplete provider — wasm side returns keyword/function suggestions. */
  complete: (ctx: CompletionContext) => CompletionResult | null;
}

export interface EditorHandle {
  destroy: () => void;
  setDoc: (text: string) => void;
}

export function createEditor(
  parent: HTMLElement,
  initial: string,
  opts: EditorOpts
): EditorHandle {
  const submitKey = {
    key: "Mod-Enter",
    preventDefault: true,
    run: () => {
      opts.onSubmit();
      return true;
    },
  };

  const state = EditorState.create({
    doc: initial,
    extensions: [
      EditorView.contentAttributes.of({ "aria-label": "Search query" }),
      lineNumbers(),
      highlightActiveLine(),
      history(),
      keymap.of([submitKey, ...defaultKeymap, ...historyKeymap]),
      linter((view) => opts.lint(view.state.doc.toString())),
      lintGutter(),
      autocompletion({
        override: [(ctx) => opts.complete(ctx)],
      }),
      EditorView.updateListener.of((update) => {
        if (update.docChanged) {
          opts.onChange(update.state.doc.toString());
        }
      }),
    ],
  });

  const view = new EditorView({ state, parent });

  return {
    destroy() {
      view.destroy();
    },
    setDoc(text: string) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: text },
      });
    },
  };
}

// Compile-time shape assertion — mirrors the `#[wasm_bindgen(module=...)]`
// extern block in `crates/trawl-web-ui/src/interop/codemirror.rs`. If the
// exported signature ever drifts (rename, added/removed parameter, return
// type change), TypeScript rejects this file before `build.sh` finishes,
// making the vendor-drift CI job fail loudly instead of shipping a runtime
// `TypeError` to the browser.
//
// The runtime-side drift check (committed bundle byte diff) catches content
// changes but NOT API changes — `createEditor` could be renamed and the
// content check would just pass the new content. This static typecheck
// covers that gap.
const _apiShape: (
  parent: HTMLElement,
  initial: string,
  opts: EditorOpts,
) => EditorHandle = createEditor;
void _apiShape;
