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
}

export interface ChartHandle {
  /** Replace the chart data with a new snapshot. */
  setData: (data: AlignedData) => void;
  /** Destroy the chart and remove it from the DOM. */
  destroy: () => void;
  /** Resize on window changes. */
  resize: (w: number, h: number) => void;
}

export function createChart(
  parent: HTMLElement,
  data: AlignedData,
  opts: ChartOpts
): ChartHandle {
  const seriesLabels = opts.series ?? [];
  const series = [
    { label: "time" },
    ...seriesLabels.map((label) => ({ label, stroke: "#f5a800", width: 2 })),
  ];

  const options: Options = {
    width: opts.width,
    height: opts.height,
    series,
    scales: {
      x: { time: true },
      y: {},
    },
    axes: [
      {},
      opts.yLabel ? { label: opts.yLabel } : {},
    ],
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
