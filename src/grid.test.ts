import { describe, expect, it } from "vitest";
import { gridCells } from "./grid";

describe("gridCells", () => {
  it("numbers the lines and centers each card along its line", () => {
    // A row of 3, then a centered row of 2.
    const boxes = [
      { across: 3, start: 0, size: 100 },
      { across: 3, start: 100, size: 100 },
      { across: 3, start: 200, size: 100 },
      { across: 148, start: 50, size: 100 },
      { across: 148, start: 150, size: 100 },
    ];
    expect(gridCells(boxes)).toEqual([
      { line: 0, center: 50 },
      { line: 0, center: 150 },
      { line: 0, center: 250 },
      { line: 1, center: 100 },
      { line: 1, center: 200 },
    ]);
  });

  it("reports nothing for no cards", () => {
    expect(gridCells([])).toEqual([]);
  });
});
