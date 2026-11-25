/** One card's place in the switcher grid, as the backend's arrows use it:
 * the line it is on (a row in the horizontal layout, a column in the
 * vertical one, which fills a column top to bottom and then wraps into the
 * next), counted from 0, and the center of the card along that line. */
export type GridCell = { line: number; center: number };

/** A card's box in layout pixels: `across` is its offset across lines
 * (`offsetTop` for rows, `offsetLeft` for columns), `start` and `size` its
 * offset and size along its line. */
export type CardBox = { across: number; start: number; size: number };

/** The grid cells of `boxes`, in card order. A new line starts at each card
 * whose `across` offset differs from the previous card's. Rows are centered,
 * so a card's index alone does not tell which card is under it: the arrows
 * compare centers instead. */
export function gridCells(boxes: readonly CardBox[]): GridCell[] {
  let line = 0;
  return boxes.map((box, index) => {
    if (index > 0 && box.across !== boxes[index - 1].across) line += 1;
    return { line, center: box.start + box.size / 2 };
  });
}
