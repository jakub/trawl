// uPlot wrapper for streaming time-series charts consumed via wasm-bindgen.
//
// Kept minimal on purpose. uPlot is tiny, fast, and schema-free; the
// heavy lifting (bucketing, aggregation) happens on trawld. This wrapper
// lets Leptos create/update/destroy charts with a handful of calls.

import uPlot, { AlignedData, Options } from "uplot";

/**
 * Chart options as the Rust side hands them over.
 *
 * Mirrored field-for-field by `Opts` in
 * `crates/trawl-web-ui/src/interop/uplot.rs`; that struct's `to_js` is
 * the only writer of this object, so a field added here is a field added
 * there.
 */
export interface ChartOpts {
  width: number;
  height: number;
  /** Optional series labels; if omitted, lines are numbered. */
  series?: string[];
  /** Optional y-axis label. */
  yLabel?: string;
  /**
   * Render style. Defaults to "line". "column" draws vertical bars,
   * "bar" the same data as horizontal bars (Column rotated).
   */
  kind?: "line" | "column" | "bar";
  /**
   * Treat x values as UTC rather than browser-local when formatting the
   * time axis. trawld emits `_time` already shifted into its configured
   * display timezone, so the caller converts those naive stamps to epoch
   * seconds as-if-UTC; rendering them browser-local would shift every
   * label by the viewer's own offset.
   */
  utc?: boolean;
  /**
   * Ordinal x scale: one label per category, xs are `0..n-1`. When set,
   * the x axis prints these labels instead of times and the hover card
   * names the category, not an interval.
   */
  xLabels?: string[];
  /** Bridge explicit nulls with a line segment. Off unless asked. */
  spanGaps?: boolean;
  /**
   * Let the chart follow its host's width itself, through its own
   * ResizeObserver. Off by default: a caller that already measures the
   * host and calls `resize()` owns the redraw, and two observers on one
   * layout change redraw the same chart twice.
   */
  observeResize?: boolean;
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
//
// Six patterns, six colours (`readColors` below): together they must cover
// `SERIES_CAP` in `crates/trawl-web-ui/src/series.rs`, the most series the
// Rust side ever hands this bridge. Raise all three together.
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

  const kind = opts.kind ?? "line";
  const bars = kind !== "line";
  // Bar is Column rotated: uPlot draws the x scale vertically and the
  // value scale horizontally. Orientation is a scale property, not a
  // path option — the bars builder, cursor and posToVal all read it
  // from `scales.x.ori` (upstream demo `scales-dir-ori.html`).
  const horizontal = kind === "bar";
  const xLabels = opts.xLabels;
  const ordinal = xLabels !== undefined;
  const seriesLabels = opts.series ?? [];

  // Time-bucketed bars sit to the RIGHT of their timestamp: a bar
  // labelled 13:00 covers [13:00, 14:00), matching how `timechart`
  // buckets. Ordinal bars are centred on their integer position. `gap`
  // keeps neighbours legible — at 24 buckets in ~320px the columns
  // otherwise anti-alias into each other and read as one solid block.
  const barPath = bars
    ? uPlot.paths.bars?.({
        align: ordinal ? 0 : 1,
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

  /**
   * The value scale keeps zero in view together with the data's own
   * extent: a flat-ish count series must not look dramatic because
   * uPlot auto-ranged the floor, and a negative metric must not vanish
   * below a zero floor. An all-zero result still gets a unit of room.
   */
  const valueRange = (_u: uPlot, min: number, max: number): [number, number] => {
    const lo = Math.min(0, min);
    let hi = Math.max(0, max);
    if (hi === lo) hi = lo + 1;
    return [lo, hi];
  };

  // Six colours, six dash patterns (`lineDashes` above): together they
  // must cover `SERIES_CAP` in `crates/trawl-web-ui/src/series.rs`.
  const readColors = () => [accent, token("--teal", accent), token("--red", accent),
    token("--yellow", accent), token("--green", accent), token("--ink", accent)];
  let colors = readColors();
  const series = [
    { label: ordinal ? "group" : opts.utc ? "UTC" : "time" },
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
        : {
            // A null bucket is a gap in the line unless the caller asks
            // otherwise; never let uPlot's default decide.
            spanGaps: opts.spanGaps ?? false,
          }),
    })),
  ];

  const axisBase = {
    stroke: () => ink3,
    grid: { stroke: () => line2, width: 1 },
    ticks: { stroke: () => line2, width: 1 },
    font: `10px ${mono}`,
  };

  // The bar charts use a hover card instead of a live legend. Keep it
  // inside the plot so a scroll container cannot clip it.
  const tooltip = bars ? document.createElement("div") : null;
  const tooltipTime = document.createElement("div");
  const tooltipValue = document.createElement("div");
  if (tooltip) {
    tooltip.className = "ig-tooltip";
    tooltip.hidden = true;
    tooltip.setAttribute("role", "tooltip");
    tooltipTime.className = "ig-tooltip-time";
    tooltip.append(tooltipTime, tooltipValue);
  }
  const formatTime = new Intl.DateTimeFormat(undefined, {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
    ...(opts.utc ? { timeZone: "UTC" } : {}),
  });
  const hideTooltip = () => { if (tooltip) tooltip.hidden = true; };
  /** Which data row the cursor is over, or -1 when it is over none. */
  const hoveredIndex = (u: uPlot, left: number, top: number): number => {
    const xs = u.data[0];
    if (xLabels) {
      // Ordinal bars are centred on integers, so the nearest integer is
      // the hovered category. Along the vertical x scale of a Bar chart
      // the cursor's `top` is the x position.
      const at = u.posToVal(horizontal ? top : left, "x");
      const index = Math.round(at);
      return index >= 0 && index < xLabels.length && Math.abs(at - index) <= 0.5 ? index : -1;
    }
    if (xs.length < 2) return -1;
    // Time bars extend right from each bucket start. uPlot's nearest
    // timestamp can select the next bucket halfway across a bar, so
    // select by interval.
    const time = u.posToVal(left, "x");
    let index = xs.length - 1;
    while (index >= 0 && xs[index] > time) index--;
    if (index < 0) return -1;
    const start = xs[index];
    const end = xs[index + 1] ?? start + (start - xs[index - 1]);
    return time >= end ? -1 : index;
  };
  const updateTooltip = (u: uPlot) => {
    if (!tooltip) return;
    const { left = -1, top = -1 } = u.cursor;
    const width = u.over.clientWidth;
    const height = u.over.clientHeight;
    if (left < 0 || top < 0 || left > width || top > height) {
      hideTooltip();
      return;
    }
    const index = hoveredIndex(u, left, top);
    if (index < 0) {
      hideTooltip();
      return;
    }
    if (xLabels) {
      tooltipTime.textContent = xLabels[index] ?? "";
    } else {
      const xs = u.data[0];
      const start = xs[index];
      const end = xs[index + 1] ?? start + (start - xs[index - 1]);
      tooltipTime.textContent = `${formatTime.format(start * 1000)} – ${formatTime.format(end * 1000)}`;
    }
    tooltipValue.textContent = seriesLabels.map((label, i) => {
      const value = u.data[i + 1][index];
      const shown = value == null ? "null" : Number(value).toLocaleString();
      return `${label.charAt(0).toUpperCase() + label.slice(1)}: ${shown}`;
    }).join(" · ");
    tooltip.hidden = false;
    tooltip.style.left = `${Math.max(0, Math.min(left + 12, width - tooltip.offsetWidth))}px`;
    tooltip.style.top = `${Math.max(0, Math.min(top + 12, height - tooltip.offsetHeight))}px`;
  };

  // Ordinal x: half a slot of padding either side so the first and last
  // bars are drawn whole, one split per category, the label as its text.
  const ordinalX = xLabels
    ? {
        scale: { time: false as const, range: [-0.5, xLabels.length - 0.5] as [number, number] },
        axis: {
          splits: () => xLabels.map((_, i) => i),
          values: (_u: uPlot, splits: number[]) => splits.map((v) => xLabels[v] ?? ""),
        },
      }
    : undefined;

  const xScale = ordinalX
    ? ordinalX.scale
    : { time: true as const, ...(bars ? { range: barRange } : {}) };

  const options: Options = {
    width: opts.width,
    height: opts.height,
    series,
    ...(opts.utc
      ? { tzDate: (ts: number) => uPlot.tzDate(new Date(ts * 1000), "Etc/UTC") }
      : {}),
    scales: {
      // Horizontal bars: x runs down the left edge (`ori: 1`), first
      // category at the top (`dir: -1`), values along the bottom.
      x: { ...xScale, ...(horizontal ? { ori: 1, dir: -1 } : {}) },
      y: { range: valueRange, ...(horizontal ? { ori: 0, dir: 1 } : {}) },
    },
    axes: [
      {
        ...axisBase,
        ...(ordinalX ? ordinalX.axis : {}),
        // Left-side category labels need more room than uPlot's default
        // axis gutter; size to the longest label, within reason.
        ...(horizontal
          ? {
              side: 3,
              size: Math.min(160, 16 + 6.5 * Math.max(0, ...(xLabels ?? []).map((l) => l.length))),
            }
          : {}),
      },
      {
        ...axisBase,
        ...(horizontal ? { side: 2 } : {}),
        ...(opts.yLabel
          ? { label: opts.yLabel, labelFont: `11px ${mono}` }
          : {}),
        // Time columns are counts: suppress uPlot's fractional ticks.
        // Line and ordinal values may be fractional, so they keep them.
        ...(bars && !ordinal
          ? {
              values: (_u: uPlot, splits: number[]) =>
                splits.map((v) => (Number.isInteger(v) ? String(v) : "")),
            }
          : {}),
      },
    ],
    cursor: {
      points: { show: !bars },
      y: !bars,
    },
    legend: { show: !bars, live: true, markers: { show: bars } },
    ...(tooltip ? {
      hooks: {
        ready: [(u: uPlot) => {
          u.over.append(tooltip);
          u.over.addEventListener("mouseleave", hideTooltip);
        }],
        setCursor: [updateTooltip],
        setData: [hideTooltip],
        setSize: [hideTooltip],
        destroy: [(u: uPlot) => {
          u.over.removeEventListener("mouseleave", hideTooltip);
          tooltip.remove();
        }],
      },
    } : {}),
  };

  const chart = new uPlot(options, data, parent);
  // Only for a caller that does not resize the chart itself: the service
  // drawer switches between a docked column and an overlay without
  // remounting, and never calls `resize()`.
  const resizeObserver = opts.observeResize ? new ResizeObserver(([entry]) => {
    const width = Math.round(entry.contentRect.width);
    if (width > 0 && width !== chart.width) {
      chart.setSize({ width, height: opts.height });
    }
  }) : null;
  resizeObserver?.observe(parent);
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
      resizeObserver?.disconnect();
      chart.destroy();
    },
    resize(w: number, h: number) {
      chart.setSize({ width: w, height: h });
    },
  };
}
