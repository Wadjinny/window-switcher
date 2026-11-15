import { describe, expect, it } from "vitest";
import { HoverGate } from "./hover-gate";

describe("HoverGate", () => {
  it("ignores the synthetic move at a stationary cursor", () => {
    const gate = new HoverGate();
    expect(gate.move(500, 300)).toBe(false);
    // Repeated moves at the same spot (no real motion) still do not count.
    expect(gate.move(500, 300)).toBe(false);
    expect(gate.allowed).toBe(false);
  });

  it("allows hover once the pointer really moves", () => {
    const gate = new HoverGate();
    gate.move(500, 300);
    expect(gate.move(501, 300)).toBe(true);
    // Stays allowed for the rest of the opening, even back at the start.
    expect(gate.move(500, 300)).toBe(true);
    expect(gate.allowed).toBe(true);
  });

  it("starts over for every opening", () => {
    const gate = new HoverGate();
    gate.move(10, 10);
    gate.move(20, 20);
    gate.reset();
    expect(gate.allowed).toBe(false);
    expect(gate.move(20, 20)).toBe(false);
    expect(gate.move(20, 21)).toBe(true);
  });
});
