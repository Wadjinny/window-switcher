import { describe, expect, it } from "vitest";
import { cardsPerLine } from "./grid";

describe("cardsPerLine", () => {
  it("counts the cards that share the first card's line", () => {
    // Rows of 3 (offsetTop), or columns of 3 (offsetLeft).
    expect(cardsPerLine([3, 3, 3, 148, 148, 148, 293, 293])).toBe(3);
  });

  it("treats a single line as one line of every card", () => {
    expect(cardsPerLine([3, 3, 3, 3])).toBe(4);
    expect(cardsPerLine([3])).toBe(1);
  });

  it("reports nothing for no cards", () => {
    expect(cardsPerLine([])).toBe(0);
  });
});
