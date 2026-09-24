// CodeMirror 6 wrapper consumed by the Leptos SPA via wasm-bindgen.
//
// Kept deliberately thin: the wasm side passes in callbacks and an
// initial document, gets back a handle with a couple of methods. All
// DSL-specific behavior (lint, autocomplete) is driven by the callbacks
// rather than baked into this bundle so the trawl-core parser stays the
// single source of truth.

import { EditorState } from "@codemirror/state";
import {
  EditorView,
  ViewPlugin,
  keymap,
  lineNumbers,
  highlightActiveLine,
} from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { linter, lintGutter, lintKeymap, Diagnostic } from "@codemirror/lint";
import {
  autocompletion,
  closeCompletion,
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
  /** Close the completion popup, if one is open. */
  closeCompletion: () => void;
}

/** The accessible name of every lint gutter marker. The marker's DOM
 * does not carry its diagnostics, and the draft diagnostic line under the
 * editor already reads them out in full, so the name says what the mark
 * is rather than repeating the message. */
const LINT_MARKER_NAME = "Query syntax error";

// CodeMirror hides the whole gutter column from assistive technology
// (`aria-hidden` on `.cm-gutters`), and its lint markers are bare divs.
// Lift the hiding onto each gutter except the lint one, so line numbers
// stay silent, and name each marker as an image. Runs in the write phase
// of a measure, after the gutter view has synced its DOM for the update
// that produced the markers.
const lintMarkerNames = ViewPlugin.fromClass(
  class {
    constructor(readonly view: EditorView) {
      this.schedule();
    }
    update() {
      this.schedule();
    }
    schedule() {
      this.view.requestMeasure({
        key: this,
        read: () => null,
        write: () => this.label(),
      });
    }
    label() {
      const dom = this.view.dom;
      for (const el of dom.querySelectorAll(".cm-gutters")) {
        el.removeAttribute("aria-hidden");
      }
      for (const el of dom.querySelectorAll(".cm-gutter:not(.cm-gutter-lint)")) {
        el.setAttribute("aria-hidden", "true");
      }
      for (const el of dom.querySelectorAll(".cm-lint-marker")) {
        el.setAttribute("role", "img");
        el.setAttribute("aria-label", LINT_MARKER_NAME);
      }
    }
  }
);

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
      // lintKeymap: F8 moves to the next diagnostic, Mod-Shift-m opens
      // the lint panel. Neither key is bound by the default, history or
      // completion keymaps.
      keymap.of([submitKey, ...defaultKeymap, ...historyKeymap, ...lintKeymap]),
      linter((view) => opts.lint(view.state.doc.toString())),
      lintGutter(),
      lintMarkerNames,
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
    closeCompletion() {
      closeCompletion(view);
    },
  };
}

// Compile-time shape assertion — mirrors the `#[wasm_bindgen(module=...)]`
// extern block in `crates/trawl-web-ui/src/interop/codemirror.rs`: the
// `createEditor` factory, and through `EditorHandle` the `destroy`,
// `setDoc` and `closeCompletion` methods the Rust side binds. If the
// exported signature ever drifts (rename, added/removed parameter, return
// type change), a TypeScript typecheck of this file rejects it.
//
// Nothing automated runs that typecheck: `build.sh` bundles with esbuild,
// which strips types without checking them, so neither the build nor the
// vendor-drift CI job (a byte diff of the committed bundle) enforces this
// assertion. Keep it in step with the Rust extern by hand; it is where a
// reader or an editor's language server sees the contract.
const _apiShape: (
  parent: HTMLElement,
  initial: string,
  opts: EditorOpts,
) => EditorHandle = createEditor;
void _apiShape;
