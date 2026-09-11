// uPlot wrapper for streaming time-series charts consumed via wasm-bindgen.
//
// Kept minimal on purpose. uPlot is tiny, fast, and schema-free; the
// heavy lifting (bucketing, aggregation) happens on trawld. This wrapper
// lets Leptos create/update/destroy charts with a handful of calls.

import uPlot, { AlignedData, Options } from "uplot";

export interface ChartOpts {
  width: number;
  height: number;
  /** Optional series labels; if omitted, lines are numbered. */
  series?: string[];
  /** Optional y-axis label. */
  yLabel?: string;
  /** Render style. Defaults to "line". */
  kind?: "line" | "bars";
  /**
   * Treat x values as UTC rather than browser-local when formatting the
   * time axis. trawld emits `_time` already shifted into its configured
   * display timezone, so the caller converts those naive stamps to epoch
   * seconds as-if-UTC; rendering them browser-local would shift every
   * label by the viewer's own offset.
   */
  utc?: boolean;
  /** Search snapshots use result positions instead of epoch seconds. */
  rowIndex?: boolean;
}

export interface ChartHandle {
  /** Replace the chart data with a new snapshot. */
  setData: (data: AlignedData) => void;
  /** Destroy the chart and remove it from the DOM. */
  destroy: () => void;
  /** Resize on window changes. */
  resize: (w: number, h: number) => void;
}

/**
 * Resolve a CSS custom property off the document root.
 *
 * The chart paints onto a canvas, so it can't inherit the Mira palette
 * the way the DOM chrome does — the tokens have to be read out and
 * handed to uPlot through color callbacks. Read on mount and whenever
 * the root theme attribute changes, without replacing the chart.
 */
function token(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return v.length > 0 ? v : fallback;
}

/**
 * Same colour at reduced alpha, as an 8-digit hex.
 *
 * Deliberately NOT `color-mix()`: this value goes to a canvas
 * `fillStyle`, whose colour parser lags the CSS engine's. `--accent` is
 * a plain hex in both themes, so the fast path always hits; anything
 * else falls through opaque rather than silently painting nothing.
 */
function translucent(color: string, alpha: number): string {
  const hex = /^#([0-9a-f]{6})$/i.exec(color);
  if (!hex) {
    return color;
  }
  const a = Math.round(alpha * 255)
    .toString(16)
    .padStart(2, "0");
  return `#${hex[1]}${a}`;
}

// Canvas and legend use the same CSS-pixel patterns. A CSS border marker
// cannot represent dash-dot patterns, so each line gets an SVG legend key.
const lineDashes = [[], [6, 4], [2, 4], [12, 4], [8, 3, 2, 3], [8, 3, 2, 3, 2, 3]];

function lineLabel(label: string, color: string, dash: number[]): HTMLElement {
  const key = document.createElement("span");
  key.className = "series-key";
  key.style.cssText = "display:inline-flex;align-items:center;gap:.4em;color:var(--ink)";
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "44");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 44 12");
  svg.setAttribute("aria-hidden", "true");
  const line = document.createElementNS(svg.namespaceURI, "line");
  line.setAttribute("x1", "0");
  line.setAttribute("x2", "44");
  line.setAttribute("y1", "6");
  line.setAttribute("y2", "6");
  line.setAttribute("stroke", color);
  line.setAttribute("stroke-width", "2");
  line.setAttribute("stroke-dasharray", dash.join(" "));
  svg.append(line);
  const text = document.createElement("span");
  text.textContent = label;
  key.append(svg, text);
  return key;
}

export function createChart(
  parent: HTMLElement,
  data: AlignedData,
  opts: ChartOpts
): ChartHandle {
  let accent = token("--accent", "#2a5c8a");
  let ink3 = token("--ink-3", "#8a8a8a");
  let line2 = token("--line-2", "rgba(128,128,128,.2)");
  const mono = token("--font-mono", "monospace");

  const bars = opts.kind === "bars";
  const seriesLabels = opts.series ?? [];

  // Bucketed bars sit to the RIGHT of their timestamp: a bar labelled
  // 13:00 covers [13:00, 14:00), matching how `timechart` buckets.
  // `gap` keeps neighbours legible — at 24 buckets in ~320px the columns
  // otherwise anti-alias into each other and read as one solid block.
  const barPath = bars
    ? uPlot.paths.bars?.({
        align: 1,
        size: [0.9, Infinity],
        gap: 2,
        radius: 0.15,
      })
    : undefined;

  /**
   * Right-align means the last bucket's bar is drawn PAST the last x
   * value, where uPlot would clip it to a sliver. Pad the x scale by one
   * bucket so the trailing bar gets its full width.
   */
  const barRange = (u: uPlot, min: number, max: number): [number, number] => {
    const xs = u.data[0];
    const step =
      xs.length > 1 ? Number(xs[xs.length - 1]) - Number(xs[xs.length - 2]) : 0;
    return [min, max + step];
  };

  const readColors = () => [accent, token("--teal", accent), token("--red", accent),
    token("--yellow", accent), token("--green", accent), token("--ink", accent)];
  let colors = readColors();
  const series = [
    { label: opts.rowIndex ? "Result position" : "time" },
    ...seriesLabels.map((label, index) => ({
      label: bars ? label : lineLabel(label, colors[index % colors.length], lineDashes[index % lineDashes.length]),
      stroke: () => bars ? accent : colors[index % colors.length],
      // uPlot passes dash lengths directly to its device-pixel canvas.
      dash: bars ? [] : lineDashes[index % lineDashes.length].map(length => length * window.devicePixelRatio),
      // Bars are flat fills with NO stroke: uPlot strokes zero-height
      // rects too, which drew a dashed hairline along the baseline for
      // every empty bucket. Flat also matches the Mira button recipe.
      width: bars ? 0 : 2,
      ...(bars
        ? {
            fill: () => translucent(accent, 0.8),
            paths: barPath,
            points: { show: false },
          }
        : {}),
    })),
  ];

  const axisBase = {
    stroke: () => ink3,
    grid: { stroke: () => line2, width: 1 },
    ticks: { stroke: () => line2, width: 1 },
    font: `10px ${mono}`,
  };

  const options: Options = {
    width: opts.width,
    height: opts.height,
    series,
    ...(opts.utc
      ? { tzDate: (ts: number) => uPlot.tzDate(new Date(ts * 1000), "Etc/UTC") }
      : {}),
    scales: {
      x: { time: !opts.rowIndex, ...(bars ? { range: barRange } : {}) },
      // Counts start at zero — letting uPlot auto-range the floor makes a
      // flat-ish series look far more dramatic than it is.
      y: { range: (_u, _min, max) => [0, Math.max(max, 1)] },
    },
    axes: [
      { ...axisBase },
      {
        ...axisBase,
        ...(opts.yLabel
          ? { label: opts.yLabel, labelFont: `11px ${mono}` }
          : {}),
        // Counts are integers; suppress uPlot's fractional ticks.
        values: (_u, splits) =>
          splits.map((v) => (Number.isInteger(v) ? String(v) : "")),
      },
    ],
    cursor: {
      points: { show: !bars },
      y: !bars,
    },
    legend: { live: true, markers: { show: bars } },
  };

  const chart = new uPlot(options, data, parent);
  let destroyed = false;
  const themeObserver = new MutationObserver(() => {
    if (destroyed) return;
    accent = token("--accent", "#2a5c8a");
    ink3 = token("--ink-3", "#8a8a8a");
    line2 = token("--line-2", "rgba(128,128,128,.2)");
    colors = readColors();
    chart.root.querySelectorAll(".series-key line").forEach((line, index) => {
      line.setAttribute("stroke", colors[index % colors.length]);
    });
    if (bars) {
      chart.root.querySelectorAll<HTMLElement>(".u-legend .u-marker").forEach((marker, index) => {
        if (index > 0) marker.style.background = translucent(accent, 0.8);
      });
    }
    // Bar paths cache their fill, so rebuild those paths at the current scales.
    // Keep the chart instance, data, native legend listeners and hidden series.
    chart.redraw(bars, true);
    // uPlot commits redraw in a microtask. Refresh hover points after its
    // cached stroke colors update, including a stationary cursor.
    queueMicrotask(() => {
      if (!destroyed) chart.setCursor({ left: chart.cursor.left ?? -10, top: chart.cursor.top ?? -10 }, false);
    });
  });
  themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

  return {
    setData(next: AlignedData) {
      chart.setData(next);
    },
    destroy() {
      destroyed = true;
      themeObserver.disconnect();
      chart.destroy();
    },
    resize(w: number, h: number) {
      chart.setSize({ width: w, height: h });
    },
  };
}
