// How far the preview cards shrink so every card fits the overlay without
// scrolling: a live thumbnail cannot be clipped by an HTML scroll container.
// Pure, so it is unit tested (preview-fit.test.ts).

import type { Layout } from "./payload";

// Mirrors `.switcher.preview .item` in style.css and PREVIEW_CARD_PITCH /
// PREVIEW_ROW_HEIGHT in overlay.rs: a card is 240px wide and 46px of chrome
// plus a 132px preview tall at full size, with a 13px gap.
const PREVIEW_CARD_WIDTH = 240;
const PREVIEW_CARD_CHROME = 46;
const PREVIEW_SLOT_HEIGHT = 132;
const PREVIEW_GAP = 13;
export const MIN_PREVIEW_SCALE = 0.3;

// Vertical preview rows (`.switcher.vertical.preview .item`, and
// VERTICAL_PREVIEW_* in overlay.rs): a 176x99 thumbnail beside a 210px label;
// 18px of border and padding around it, 8px between rows, 13px between
// columns.
const VERTICAL_THUMB_WIDTH = 176;
const VERTICAL_THUMB_HEIGHT = 99;
const VERTICAL_ROW_CHROME = 18;
const VERTICAL_ROW_LABEL = 210;
const VERTICAL_ROW_GAP = 8;
const VERTICAL_COLUMN_GAP = 13;

/** Vertical layout: rows fill a column top to bottom, then wrap into the
 * next column, so find the largest scale whose columns fit the width. */
export function verticalPreviewFits(
  scale: number,
  count: number,
  width: number,
  height: number,
): boolean {
  const rowHeight = VERTICAL_ROW_CHROME + VERTICAL_THUMB_HEIGHT * scale;
  const rowWidth = VERTICAL_ROW_CHROME + VERTICAL_ROW_LABEL + VERTICAL_THUMB_WIDTH * scale;
  const rows = Math.max(
    1,
    Math.floor((height + VERTICAL_ROW_GAP + 0.5) / (rowHeight + VERTICAL_ROW_GAP)),
  );
  const columns = Math.ceil(count / rows);
  return columns * rowWidth + (columns - 1) * VERTICAL_COLUMN_GAP <= width + 0.5;
}

/** Horizontal layout: cards fill a row, then wrap into the next row. */
export function horizontalPreviewFits(
  scale: number,
  count: number,
  width: number,
  height: number,
): boolean {
  const cardWidth = PREVIEW_CARD_WIDTH * scale;
  const cardHeight = PREVIEW_CARD_CHROME + PREVIEW_SLOT_HEIGHT * scale;
  const columns = Math.max(1, Math.floor((width + PREVIEW_GAP) / (cardWidth + PREVIEW_GAP)));
  const rows = Math.ceil(count / columns);
  return rows * cardHeight + (rows - 1) * PREVIEW_GAP <= height + 0.5;
}

/** The largest scale (1 down to MIN_PREVIEW_SCALE, in 0.025 steps) at which
 * `count` cards fit a `width` x `height` list. */
export function previewScale(layout: Layout, count: number, width: number, height: number): number {
  const fits = layout === "vertical" ? verticalPreviewFits : horizontalPreviewFits;
  let scale = 1;
  for (; scale > MIN_PREVIEW_SCALE; scale -= 0.025) {
    if (fits(scale, count, width, height)) break;
  }
  return Math.max(scale, MIN_PREVIEW_SCALE);
}
