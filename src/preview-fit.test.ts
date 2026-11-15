import { describe, expect, it } from "vitest";
import {
  MIN_PREVIEW_SCALE,
  horizontalPreviewFits,
  previewScale,
  verticalPreviewFits,
} from "./preview-fit";

describe("verticalPreviewFits", () => {
  // Full-size rows are 404x117 with 8px between rows, 13px between columns.
  it("fills a column before wrapping into the next", () => {
    // 625px holds 5 rows: one column.
    expect(verticalPreviewFits(1, 5, 404, 625)).toBe(true);
    expect(verticalPreviewFits(1, 5, 403, 625)).toBe(false);
    // 600px holds 4 rows: two columns of 404 plus a 13px gap.
    expect(verticalPreviewFits(1, 5, 821, 600)).toBe(true);
    expect(verticalPreviewFits(1, 5, 820, 600)).toBe(false);
  });

  it("smaller rows fit more per column", () => {
    expect(verticalPreviewFits(1, 8, 404, 600)).toBe(false);
    expect(verticalPreviewFits(0.5, 8, 404, 600)).toBe(true);
  });

  it("always counts at least one row", () => {
    expect(verticalPreviewFits(1, 2, 821, 10)).toBe(true);
    expect(verticalPreviewFits(1, 3, 821, 10)).toBe(false);
  });
});

describe("horizontalPreviewFits", () => {
  // Full-size cards are 240x178 with a 13px gap.
  it("fills a row before wrapping into the next", () => {
    expect(horizontalPreviewFits(1, 4, 1000, 178)).toBe(true);
    expect(horizontalPreviewFits(1, 4, 1000, 177)).toBe(false);
    expect(horizontalPreviewFits(1, 8, 1000, 369)).toBe(true);
    expect(horizontalPreviewFits(1, 8, 1000, 368)).toBe(false);
  });
});

describe("previewScale", () => {
  it("keeps full size when everything fits", () => {
    expect(previewScale("horizontal", 4, 1100, 200)).toBe(1);
    expect(previewScale("vertical", 5, 404, 625)).toBe(1);
  });

  it("finds the largest step that fits", () => {
    for (const [layout, count, width, height] of [
      ["horizontal", 8, 1000, 300],
      ["horizontal", 20, 1800, 500],
      ["vertical", 12, 900, 500],
      ["vertical", 30, 1800, 900],
    ] as const) {
      const fits = layout === "vertical" ? verticalPreviewFits : horizontalPreviewFits;
      const scale = previewScale(layout, count, width, height);
      expect(scale).toBeLessThan(1);
      expect(scale).toBeGreaterThan(MIN_PREVIEW_SCALE);
      expect(fits(scale, count, width, height)).toBe(true);
      expect(fits(scale + 0.025, count, width, height)).toBe(false);
    }
  });

  it("never shrinks below the minimum", () => {
    expect(previewScale("horizontal", 200, 300, 200)).toBe(MIN_PREVIEW_SCALE);
    expect(previewScale("vertical", 200, 300, 200)).toBe(MIN_PREVIEW_SCALE);
  });
});
