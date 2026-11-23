// Validation of the events and command results the backend sends to the
// overlay. Everything here is pure, so it is unit tested (payload.test.ts).

export type Mode = "window" | "app";
export type CardStyle = "icon" | "preview";
export type Layout = "horizontal" | "vertical";

/** Theme colors from the app's icon: the card fill and its border. */
export type AppColors = { fill: string; border: string };

export type SwitchItem = {
  id: number;
  title: string;
  subtitle: string;
  icon: string | null;
  accent: string;
  colors: AppColors | null;
  window_count: number;
  /** Preview style: the backend draws a live thumbnail of window `id` here. */
  thumbnail: boolean;
};

export type SwitcherPayload = {
  mode: Mode;
  selected: number;
  items: SwitchItem[];
  hint: string;
  card_style: CardStyle;
  layout: Layout;
  generation: number;
  /** Latency logging is on in the backend: report render and paint times. */
  timing: boolean;
  /** Latency logging only: Unix ms when the backend sent this event. */
  sent_at: number | null;
};

export type SelectionPayload = { generation: number; selected: number; sent_at: number | null };

export const DEFAULT_ACCENT = "#2678d9";
export const DEFAULT_HINTS: Record<Mode, string> = {
  window: "Alt + `",
  app: "Win + Tab",
};

// Icons are untrusted input: only allow raster data URIs (never SVG or a
// remote URL), and cap their size so a bad payload cannot bloat the DOM.
const ICON_DATA_URI =
  /^data:image\/(?:png|jpe?g|webp|gif|bmp|x-icon|vnd\.microsoft\.icon)(?:;[a-z0-9.+-]+=[^;,]*)*;base64,[a-z0-9+/=\s]+$/i;
export const MAX_ICON_LENGTH = 1_000_000;

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The icon as an image source, or null when it is not a safe raster data URI. */
export function checkIconSource(icon: string): string | null {
  const source = icon.trim();
  return source.length > 0 && source.length <= MAX_ICON_LENGTH && ICON_DATA_URI.test(source)
    ? source
    : null;
}

const HEX_COLOR = /^#[0-9a-f]{6}$/i;

/** The theme colors, or null unless both are `#rrggbb` (they end up in CSS). */
export function parseColors(value: unknown): AppColors | null {
  if (!isRecord(value)) return null;
  const { fill, border } = value;
  if (typeof fill !== "string" || typeof border !== "string") return null;
  return HEX_COLOR.test(fill) && HEX_COLOR.test(border) ? { fill, border } : null;
}

export function parseItem(value: unknown): SwitchItem | null {
  if (!isRecord(value)) return null;
  const { id } = value;
  if (typeof id !== "number" || !Number.isFinite(id)) return null;

  const rawTitle = typeof value.title === "string" ? value.title : "";
  const rawSubtitle = typeof value.subtitle === "string" ? value.subtitle : "";
  const title = rawTitle || rawSubtitle;
  if (!title) return null;

  const accent =
    typeof value.accent === "string" && value.accent.trim().length > 0
      ? value.accent.trim()
      : DEFAULT_ACCENT;

  const rawCount = value.window_count;
  const windowCount =
    typeof rawCount === "number" && Number.isFinite(rawCount)
      ? Math.max(1, Math.trunc(rawCount))
      : 1;

  return {
    id,
    title,
    subtitle: rawSubtitle,
    icon: typeof value.icon === "string" ? value.icon : null,
    accent,
    colors: parseColors(value.colors),
    window_count: windowCount,
    thumbnail: value.thumbnail === true,
  };
}

export function parsePayload(value: unknown): SwitcherPayload | null {
  if (!isRecord(value)) return null;
  const { mode } = value;
  if (mode !== "window" && mode !== "app") return null;

  const rawItems = value.items;
  if (!Array.isArray(rawItems)) return null;

  const items: SwitchItem[] = [];
  for (const entry of rawItems) {
    const item = parseItem(entry);
    // One malformed row means the selection index can no longer be trusted.
    if (!item) return null;
    items.push(item);
  }

  const rawSelected =
    typeof value.selected === "number" && Number.isFinite(value.selected)
      ? Math.trunc(value.selected)
      : 0;
  const selected =
    items.length === 0 ? 0 : Math.min(Math.max(rawSelected, 0), items.length - 1);

  const hint =
    typeof value.hint === "string" && value.hint.trim().length > 0
      ? value.hint
      : DEFAULT_HINTS[mode];

  const card_style: CardStyle = value.card_style === "preview" ? "preview" : "icon";
  const layout: Layout = value.layout === "vertical" ? "vertical" : "horizontal";
  const generation =
    typeof value.generation === "number" && Number.isFinite(value.generation)
      ? value.generation
      : 0;

  const timing = value.timing === true;
  const sent_at = typeof value.sent_at === "number" ? value.sent_at : null;

  return { mode, selected, items, hint, card_style, layout, generation, timing, sent_at };
}

export function parseSelection(value: unknown): SelectionPayload | null {
  if (!isRecord(value)) return null;
  const { generation, selected } = value;
  if (typeof generation !== "number" || !Number.isFinite(generation)) return null;
  if (typeof selected !== "number" || !Number.isInteger(selected)) return null;
  const sent_at = typeof value.sent_at === "number" ? value.sent_at : null;
  return { generation, selected, sent_at };
}
