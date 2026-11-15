import { describe, expect, it } from "vitest";
import {
  DEFAULT_ACCENT,
  MAX_ICON_LENGTH,
  checkIconSource,
  parseItem,
  parsePayload,
  parseSelection,
} from "./payload";

const PNG = "data:image/png;base64,iVBORw0KGgo=";

function item(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: 42,
    title: "Inbox - Mail",
    subtitle: "Mail",
    icon: PNG,
    accent: "#8b54d6",
    window_count: 2,
    thumbnail: true,
    ...overrides,
  };
}

/** A payload exactly as `SwitcherPayload` in ipc.rs serializes it. */
function payload(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    mode: "app",
    selected: 1,
    items: [item(), item({ id: 43, title: "Notes", subtitle: "Editor" })],
    hint: "Win + Tab",
    card_style: "preview",
    layout: "vertical",
    generation: 7,
    ...overrides,
  };
}

describe("parsePayload", () => {
  it("accepts the backend's payload", () => {
    expect(parsePayload(payload())).toEqual({
      mode: "app",
      selected: 1,
      items: [
        {
          id: 42,
          title: "Inbox - Mail",
          subtitle: "Mail",
          icon: PNG,
          accent: "#8b54d6",
          window_count: 2,
          thumbnail: true,
        },
        {
          id: 43,
          title: "Notes",
          subtitle: "Editor",
          icon: PNG,
          accent: "#8b54d6",
          window_count: 2,
          thumbnail: true,
        },
      ],
      hint: "Win + Tab",
      card_style: "preview",
      layout: "vertical",
      generation: 7,
      // `timing` and `sent_at` are skipped by the backend unless timing is on.
      timing: false,
      sent_at: null,
    });
  });

  it("keeps the latency fields when timing is on", () => {
    const parsed = parsePayload(payload({ timing: true, sent_at: 1234.5 }));
    expect(parsed?.timing).toBe(true);
    expect(parsed?.sent_at).toBe(1234.5);
  });

  it("rejects anything that is not a payload", () => {
    for (const value of [null, undefined, 3, "x", [], [payload()]]) {
      expect(parsePayload(value)).toBeNull();
    }
    expect(parsePayload(payload({ mode: "desktop" }))).toBeNull();
    expect(parsePayload(payload({ items: "none" }))).toBeNull();
  });

  it("rejects the whole payload when one item is malformed", () => {
    expect(parsePayload(payload({ items: [item(), item({ id: "43" })] }))).toBeNull();
    expect(parsePayload(payload({ items: [item({ title: "", subtitle: "" })] }))).toBeNull();
  });

  it("clamps the selection into the items", () => {
    expect(parsePayload(payload({ selected: 9 }))?.selected).toBe(1);
    expect(parsePayload(payload({ selected: -3 }))?.selected).toBe(0);
    expect(parsePayload(payload({ selected: 1.9 }))?.selected).toBe(1);
    expect(parsePayload(payload({ selected: Number.NaN }))?.selected).toBe(0);
    expect(parsePayload(payload({ selected: 5, items: [] }))?.selected).toBe(0);
  });

  it("falls back for unknown styles, layouts, hints and generations", () => {
    const parsed = parsePayload(
      payload({ card_style: "huge", layout: "diagonal", hint: "  ", generation: "x", mode: "window" }),
    );
    expect(parsed?.card_style).toBe("icon");
    expect(parsed?.layout).toBe("horizontal");
    expect(parsed?.hint).toBe("Alt + `");
    expect(parsed?.generation).toBe(0);
    expect(parsePayload(payload({ hint: undefined }))?.hint).toBe("Win + Tab");
  });
});

describe("parseItem", () => {
  it("uses the subtitle when the title is empty", () => {
    expect(parseItem(item({ title: "" }))?.title).toBe("Mail");
  });

  it("normalizes accent, count, icon and thumbnail", () => {
    expect(
      parseItem(item({ accent: "  ", window_count: 0, icon: 5, thumbnail: "yes" })),
    ).toMatchObject({ accent: DEFAULT_ACCENT, window_count: 1, icon: null, thumbnail: false });
    expect(parseItem(item({ accent: " #123456 ", window_count: 3.7 }))).toMatchObject({
      accent: "#123456",
      window_count: 3,
    });
    expect(parseItem(item({ window_count: Number.POSITIVE_INFINITY }))?.window_count).toBe(1);
  });

  it("requires a finite numeric id", () => {
    expect(parseItem(item({ id: Number.NaN }))).toBeNull();
    expect(parseItem(item({ id: undefined }))).toBeNull();
  });
});

describe("parseSelection", () => {
  it("accepts the backend's selection event", () => {
    expect(parseSelection({ generation: 3, selected: 2 })).toEqual({
      generation: 3,
      selected: 2,
      sent_at: null,
    });
    expect(parseSelection({ generation: 3, selected: 0, sent_at: 9 })?.sent_at).toBe(9);
  });

  it("rejects fractional or missing fields", () => {
    expect(parseSelection({ generation: 3, selected: 1.5 })).toBeNull();
    expect(parseSelection({ selected: 1 })).toBeNull();
    expect(parseSelection({ generation: Number.POSITIVE_INFINITY, selected: 1 })).toBeNull();
    expect(parseSelection("3")).toBeNull();
  });
});

describe("checkIconSource", () => {
  it("allows raster data URIs", () => {
    expect(checkIconSource(PNG)).toBe(PNG);
    expect(checkIconSource(`  ${PNG}\n`)).toBe(PNG);
    expect(checkIconSource("data:image/jpeg;base64,/9j/4AAQ")).not.toBeNull();
    expect(checkIconSource("data:image/vnd.microsoft.icon;base64,AAAB")).not.toBeNull();
  });

  it("rejects SVG, remote URLs, scripts and oversized data", () => {
    for (const icon of [
      "",
      "data:image/svg+xml;base64,PHN2Zz4=",
      "https://example.com/icon.png",
      "javascript:alert(1)",
      "data:image/png,not-base64",
      "data:text/html;base64,PGI+",
      `data:image/png;base64,${"A".repeat(MAX_ICON_LENGTH)}`,
    ]) {
      expect(checkIconSource(icon)).toBeNull();
    }
  });
});
