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
 * handed to uPlot as literal colours. Resolved once at construction;
 * charts are rebuilt when their host component remounts.
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

export function createChart(
  parent: HTMLElement,
  data: AlignedData,
  opts: ChartOpts
): ChartHandle {
  const accent = token("--accent", "#2a5c8a");
  const ink3 = token("--ink-3", "#8a8a8a");
  const line2 = token("--line-2", "rgba(128,128,128,.2)");
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

  const series = [
    { label: "time" },
    ...seriesLabels.map((label) => ({
      label,
      stroke: accent,
      // Bars are flat fills with NO stroke: uPlot strokes zero-height
      // rects too, which drew a dashed hairline along the baseline for
      // every empty bucket. Flat also matches the Mira button recipe.
      width: bars ? 0 : 2,
      ...(bars
        ? {
            fill: translucent(accent, 0.8),
            paths: barPath,
            points: { show: false },
          }
        : {}),
    })),
  ];

  const axisBase = {
    stroke: ink3,
    grid: { stroke: line2, width: 1 },
    ticks: { stroke: line2, width: 1 },
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
      x: { time: true, ...(bars ? { range: barRange } : {}) },
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
    legend: { live: true },
  };

  const chart = new uPlot(options, data, parent);

  return {
    setData(next: AlignedData) {
      chart.setData(next);
    },
    destroy() {
      chart.destroy();
    },
    resize(w: number, h: number) {
      chart.setSize({ width: w, height: h });
    },
  };
}
