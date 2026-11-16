/** How many cards make up the first line of the switcher grid: a row in the
 * horizontal layout, a column in the vertical one (which fills a column top
 * to bottom, then wraps into the next). `offsets` are the cards' offsets
 * across lines, in card order: their `offsetTop` for rows, their
 * `offsetLeft` for columns. The first line ends at the first card whose
 * offset differs from the first card's. */
export function cardsPerLine(offsets: readonly number[]): number {
  if (offsets.length === 0) return 0;
  const first = offsets[0];
  let count = 1;
  while (count < offsets.length && offsets[count] === first) count += 1;
  return count;
}
