// Cytoscape.js wrapper for linkage-graph visualization consumed via wasm-bindgen.
//
// Mirrors the uplot.ts pattern: thin lifecycle handle that Leptos creates,
// updates, and destroys. All styling is canvas-based (no external CSS).

import cytoscape, { Core, ElementDefinition } from "cytoscape";

export interface GraphElement {
  group: "nodes" | "edges";
  data: Record<string, unknown>;
  classes?: string;
}

export interface GraphOpts {
  width: number;
  height: number;
}

export interface GraphHandle {
  setElements: (elements: GraphElement[]) => void;
  destroy: () => void;
  resize: () => void;
  fit: () => void;
  onNodeClick: (cb: (nodeId: string, nodeType: string) => void) => void;
}

function cssVar(el: HTMLElement, name: string): string {
  return getComputedStyle(el).getPropertyValue(name).trim() || "#888";
}

function buildStyle(container: HTMLElement): cytoscape.Stylesheet[] {
  const amber = cssVar(container, "--amber");
  const blue = cssVar(container, "--blue");
  const teal = cssVar(container, "--teal");
  const green = cssVar(container, "--green");
  const red = cssVar(container, "--red");
  const yellow = cssVar(container, "--yellow");
  const ink2 = cssVar(container, "--ink-2");
  const ink3 = cssVar(container, "--ink-3");
  const ink4 = cssVar(container, "--ink-4");
  const panelBg = cssVar(container, "--panel");

  return [
    {
      selector: "node",
      style: {
        label: "data(label)",
        "text-valign": "bottom",
        "text-halign": "center",
        "font-size": "10px",
        color: ink2,
        "text-margin-y": 4,
        "background-color": ink3,
        "text-max-width": "90px",
        "text-wrap": "ellipsis",
      },
    },
    {
      selector: "node.story-central",
      style: {
        shape: "round-rectangle",
        "background-color": amber,
        width: 60,
        height: 40,
        "font-size": "11px",
        "font-weight": 600,
        "text-max-width": "120px",
      },
    },
    {
      selector: "node.story-related",
      style: {
        shape: "round-rectangle",
        "background-color": blue,
        width: 40,
        height: 28,
      },
    },
    {
      selector: "node.claim",
      style: {
        shape: "ellipse",
        width: 26,
        height: 26,
      },
    },
    {
      selector: "node.claim-evidence",
      style: { "background-color": green },
    },
    {
      selector: "node.claim-contradiction",
      style: { "background-color": red },
    },
    {
      selector: "node.claim-supersession, node.claim-correction",
      style: { "background-color": yellow },
    },
    {
      selector: "node.claim-evolution",
      style: { "background-color": amber },
    },
    {
      selector:
        "node.claim-related, node.claim-background, node.claim-duplicate, node.claim-new_story",
      style: { "background-color": ink3 },
    },
    {
      selector: "node.entity",
      style: {
        shape: "diamond",
        "background-color": teal,
        width: 32,
        height: 32,
      },
    },
    {
      selector: "node.source",
      style: {
        shape: "triangle",
        "background-color": ink2,
        width: 28,
        height: 28,
      },
    },
    {
      selector: "edge",
      style: {
        width: 1.5,
        "line-color": ink4,
        "target-arrow-color": ink4,
        "target-arrow-shape": "triangle",
        "arrow-scale": 0.6,
        "curve-style": "bezier",
        label: "data(label)",
        "font-size": "8px",
        color: ink3,
        "text-rotation": "autorotate",
        "text-margin-y": -8,
        "text-opacity": 0.7,
      },
    },
    {
      selector: "edge.edge-evidence",
      style: { "line-color": green, "target-arrow-color": green },
    },
    {
      selector: "edge.edge-contradiction",
      style: {
        "line-color": red,
        "target-arrow-color": red,
        "line-style": "dashed",
      },
    },
    {
      selector: "edge.edge-supersession, edge.edge-correction",
      style: {
        "line-color": yellow,
        "target-arrow-color": yellow,
        "line-style": "dashed",
      },
    },
    {
      selector: "edge.edge-evolution",
      style: { "line-color": amber, "target-arrow-color": amber },
    },
    {
      selector: "edge.edge-story-relation",
      style: { "line-color": blue, "target-arrow-color": blue },
    },
    {
      selector: "edge.edge-subject",
      style: {
        "line-color": teal,
        "target-arrow-color": teal,
        opacity: 0.5,
      },
    },
    {
      selector: "edge.edge-object",
      style: {
        "line-color": teal,
        "target-arrow-color": teal,
        opacity: 0.3,
      },
    },
    {
      selector: "edge.edge-source",
      style: {
        "line-color": ink4,
        "target-arrow-color": ink4,
        "line-style": "dashed",
        opacity: 0.4,
      },
    },
    {
      selector: "node:active",
      style: { "overlay-color": amber, "overlay-opacity": 0.15 },
    },
    {
      selector: "node:selected",
      style: {
        "border-width": 2,
        "border-color": amber,
      },
    },
  ];
}

export function createGraph(
  container: HTMLElement,
  elements: GraphElement[],
  opts: GraphOpts
): GraphHandle {
  container.style.width = opts.width + "px";
  container.style.height = opts.height + "px";

  const cy: Core = cytoscape({
    container,
    elements: elements as ElementDefinition[],
    style: buildStyle(container),
    layout: {
      name: "cose",
      animate: false,
      nodeRepulsion: () => 8000,
      idealEdgeLength: () => 80,
      gravity: 0.3,
      padding: 30,
    } as any,
    minZoom: 0.3,
    maxZoom: 3,
    wheelSensitivity: 0.3,
  });

  return {
    setElements(next: GraphElement[]) {
      cy.elements().remove();
      cy.add(next as ElementDefinition[]);
      cy.layout({
        name: "cose",
        animate: false,
        nodeRepulsion: () => 8000,
        idealEdgeLength: () => 80,
        gravity: 0.3,
        padding: 30,
      } as any).run();
      cy.fit(undefined, 30);
    },

    destroy() {
      cy.destroy();
    },

    resize() {
      cy.resize();
      cy.fit(undefined, 30);
    },

    fit() {
      cy.fit(undefined, 30);
    },

    onNodeClick(cb: (nodeId: string, nodeType: string) => void) {
      cy.on("tap", "node", (evt) => {
        const node = evt.target;
        const id = node.data("id") as string;
        const nodeType = (node.data("node_type") as string) || "unknown";
        cb(id, nodeType);
      });
    },
  };
}
