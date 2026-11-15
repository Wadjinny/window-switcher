import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import "./style.css";

type Mode = "window" | "app";
type CardStyle = "icon" | "preview";
type Layout = "horizontal" | "vertical";

type SwitchItem = {
  id: number;
  title: string;
  subtitle: string;
  icon: string | null;
  accent: string;
  window_count: number;
  /** Preview style: the backend draws a live thumbnail of window `id` here. */
  thumbnail: boolean;
};

type SwitcherPayload = {
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

const app = document.querySelector<HTMLElement>("#app")!;

const DEFAULT_ACCENT = "#2678d9";
const DEFAULT_HINTS: Record<Mode, string> = {
  window: "Alt + `",
  app: "Win + Tab",
};

// Icons are untrusted input: only allow raster data URIs (never SVG or a
// remote URL), and cap their size so a bad payload cannot bloat the DOM.
const ICON_DATA_URI =
  /^data:image\/(?:png|jpe?g|webp|gif|bmp|x-icon|vnd\.microsoft\.icon)(?:;[a-z0-9.+-]+=[^;,]*)*;base64,[a-z0-9+/=\s]+$/i;
const MAX_ICON_LENGTH = 1_000_000;

// ---------------------------------------------------------------------------
// Payload validation
// ---------------------------------------------------------------------------

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseItem(value: unknown): SwitchItem | null {
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
    window_count: windowCount,
    thumbnail: value.thumbnail === true,
  };
}

function parsePayload(value: unknown): SwitcherPayload | null {
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

// The same icon strings arrive with every opening; checking a large data URI
// against the pattern each time is wasted work, so results are remembered.
const MAX_REMEMBERED_ICONS = 256;
const checkedIcons = new Map<string, string | null>();
/** Icon sources that have decoded once: later cards can draw them at once. */
const decodedIcons = new Set<string>();

function remember<T>(cache: Map<string, T> | Set<string>, key: string, value?: T): void {
  if (cache.size >= MAX_REMEMBERED_ICONS) cache.clear();
  if (cache instanceof Map) cache.set(key, value as T);
  else cache.add(key);
}

function safeIconSource(icon: string | null): string | null {
  if (icon === null) return null;
  const known = checkedIcons.get(icon);
  if (known !== undefined) return known;
  const source = icon.trim();
  const safe =
    source.length > 0 && source.length <= MAX_ICON_LENGTH && ICON_DATA_URI.test(source)
      ? source
      : null;
  remember(checkedIcons, icon, safe);
  return safe;
}

function initialLetter(value: string): string {
  const trimmed = value.trim();
  if (trimmed.length === 0) return "■";
  const [first] = Array.from(trimmed);
  return first ? first.toUpperCase() : "■";
}

// ---------------------------------------------------------------------------
// DOM view
// ---------------------------------------------------------------------------

type CardView = {
  element: HTMLElement;
  shell: HTMLElement;
  fallback: HTMLElement;
  titleElement: HTMLElement;
  titleText: Text;
  subtitleElement: HTMLElement;
  subtitleText: Text;
  countElement: HTMLElement;
  countText: Text;
  image: HTMLImageElement | null;
  iconSource: string | null;
  failedSource: string | null;
  iconRequest: number;
  /** Window handle; the backend's preview slots are keyed by it. */
  id: number;
  /** Preview style only: the slot the live thumbnail is drawn into. */
  thumb: HTMLElement | null;
  /** Preview style only: shown in the slot when there is no thumbnail. */
  thumbFallback: HTMLElement | null;
  thumbIcon: HTMLImageElement | null;
  thumbnail: boolean;
};

type View = {
  key: string;
  style: CardStyle;
  layout: Layout;
  generation: number;
  section: HTMLElement;
  items: HTMLElement;
  labelText: Text;
  hintText: Text;
  kbd: HTMLElement;
  cards: CardView[];
};

let current: View | null = null;
/** Latency logging only: icon decodes started by the payload being rendered. */
const pendingIconDecodes: Promise<void>[] = [];
let timingEnabled = false;
let unlisten: UnlistenFn | null = null;
let receivedState = false;
let disposed = false;

function setCardIcon(card: CardView, icon: string | null): void {
  const source = safeIconSource(icon);

  if (source === null) {
    if (card.thumbIcon) card.thumbIcon.hidden = true;
    if (card.iconSource === null && card.image === null) {
      card.fallback.hidden = false;
      return;
    }
    card.iconSource = null;
    card.failedSource = null;
    // Invalidate any decode still in flight for the old icon.
    card.iconRequest += 1;
    card.image?.remove();
    card.image = null;
    card.fallback.hidden = false;
    return;
  }

  if (source === card.iconSource || source === card.failedSource) return;

  card.iconSource = source;
  const request = ++card.iconRequest;

  const show = (image: HTMLImageElement): void => {
    card.image?.remove();
    card.image = image;
    image.hidden = false;
    card.shell.append(image);
    card.fallback.hidden = true;
    card.failedSource = null;
    if (card.thumbIcon) {
      card.thumbIcon.src = source;
      card.thumbIcon.hidden = false;
    }
  };

  if (decodedIcons.has(source)) {
    // Already decoded for an earlier card: the image cache has the bitmap, so
    // it is drawn in the same frame as the card instead of a frame later.
    const image = document.createElement("img");
    image.alt = "";
    image.src = source;
    show(image);
    return;
  }

  // Keep the letter tile visible until the bitmap really decodes: the image is
  // only attached to the DOM after decode() resolves, so there is no flicker
  // when the backend fills icons in a second event.
  const image = document.createElement("img");
  image.alt = "";
  image.decoding = "async";
  image.src = source;

  const decoded = image
    .decode()
    .then(() => {
      remember(decodedIcons, source);
      if (request !== card.iconRequest) return;
      show(image);
    })
    .catch(() => {
      if (request !== card.iconRequest) return;
      card.iconSource = null;
      card.failedSource = source;
      card.fallback.hidden = card.image !== null;
    });
  if (timingEnabled) pendingIconDecodes.push(decoded);
}

function setCardData(card: CardView, item: SwitchItem, mode: Mode): void {
  if (card.titleText.data !== item.title) card.titleText.data = item.title;
  card.titleElement.title = item.title;

  if (card.subtitleText.data !== item.subtitle) {
    card.subtitleText.data = item.subtitle;
  }
  card.subtitleElement.title = item.subtitle;
  // Vertical rows center the title when there is no subtitle under it.
  card.element.classList.toggle("no-subtitle", item.subtitle.length === 0);

  card.fallback.style.setProperty("--accent", item.accent);
  const letter = initialLetter(item.title || item.subtitle);
  if (card.fallback.textContent !== letter) card.fallback.textContent = letter;

  const showCount = mode === "app" && item.window_count > 1;
  card.countElement.hidden = !showCount;
  if (showCount) {
    if (card.countText.data !== String(item.window_count)) {
      card.countText.data = String(item.window_count);
    }
    card.countElement.title = `${item.window_count} windows`;
  } else {
    card.countElement.removeAttribute("title");
  }

  card.id = item.id;
  card.thumbnail = item.thumbnail;
  if (card.thumbFallback) card.thumbFallback.hidden = item.thumbnail;

  setCardIcon(card, item.icon);
}

/** Preview slot: the backend draws the thumbnail over it, or, for a
 * minimized window, the app icon is shown instead. */
function createThumb(): {
  thumb: HTMLElement;
  thumbFallback: HTMLElement;
  thumbIcon: HTMLImageElement;
} {
  const thumb = document.createElement("div");
  thumb.className = "thumb";

  const thumbFallback = document.createElement("div");
  thumbFallback.className = "thumb-fallback";
  const thumbIcon = document.createElement("img");
  thumbIcon.alt = "";
  thumbIcon.hidden = true;
  const note = document.createElement("span");
  note.textContent = "Minimized";
  thumbFallback.append(thumbIcon, note);
  thumb.append(thumbFallback);
  return { thumb, thumbFallback, thumbIcon };
}

function createCard(item: SwitchItem, mode: Mode, style: CardStyle): CardView {
  const element = document.createElement("article");
  element.className = "item";
  element.setAttribute("role", "option");
  element.setAttribute("aria-selected", "false");
  element.id = `switcher-item-${item.id}`;

  const ring = document.createElement("div");
  ring.className = "focus-ring";

  const shell = document.createElement("div");
  shell.className = "icon-shell";

  const fallback = document.createElement("span");
  fallback.className = "fallback-icon";
  shell.append(fallback);

  const titleElement = document.createElement("p");
  titleElement.className = "title";
  titleElement.dir = "auto";
  const titleText = document.createTextNode("");
  titleElement.append(titleText);

  const subtitleElement = document.createElement("span");
  subtitleElement.className = "subtitle";
  subtitleElement.dir = "auto";
  const subtitleText = document.createTextNode("");
  subtitleElement.append(subtitleText);

  const countElement = document.createElement("span");
  countElement.className = "window-count";
  countElement.hidden = true;
  const countText = document.createTextNode("");
  countElement.append(countText);

  let preview: ReturnType<typeof createThumb> | null = null;
  if (style === "preview") {
    // Like the built-in Alt+Tab: the live preview, then a small icon, the
    // window title and the window count underneath.
    preview = createThumb();
    const labelRow = document.createElement("div");
    labelRow.className = "label-row";
    labelRow.append(shell, titleElement, countElement);
    element.append(preview.thumb, labelRow);
  } else {
    ring.append(shell);
    element.append(ring, titleElement, subtitleElement, countElement);
  }

  const card: CardView = {
    element,
    shell,
    fallback,
    titleElement,
    titleText,
    subtitleElement,
    subtitleText,
    countElement,
    countText,
    image: null,
    iconSource: null,
    failedSource: null,
    iconRequest: 0,
    id: item.id,
    thumb: preview?.thumb ?? null,
    thumbFallback: preview?.thumbFallback ?? null,
    thumbIcon: preview?.thumbIcon ?? null,
    thumbnail: item.thumbnail,
  };

  setCardData(card, item, mode);
  return card;
}

function captionFor(payload: SwitcherPayload): string {
  if (payload.items.length === 0) return "No open windows";
  return payload.mode === "window"
    ? "Windows of the current app"
    : "Open applications";
}

function payloadKey(payload: SwitcherPayload): string {
  const ids = payload.items.map((item) => item.id).join(",");
  return `${payload.card_style}:${payload.layout}:${payload.mode}:${ids}`;
}

function buildView(payload: SwitcherPayload): View {
  const label = captionFor(payload);

  const section = document.createElement("section");
  section.className = payload.items.length === 0 ? "switcher empty" : "switcher";
  section.classList.toggle("preview", payload.card_style === "preview");
  section.classList.toggle("vertical", payload.layout === "vertical");
  section.setAttribute("aria-label", label);

  const caption = document.createElement("div");
  caption.className = "caption";
  const labelElement = document.createElement("span");
  const labelText = document.createTextNode(label);
  labelElement.append(labelText);

  const kbd = document.createElement("kbd");
  const hintText = document.createTextNode(payload.hint);
  kbd.append(hintText);
  kbd.hidden = payload.items.length === 0;
  caption.append(labelElement, kbd);

  const items = document.createElement("div");
  items.className = "items";
  items.setAttribute("role", "listbox");
  items.setAttribute("aria-label", label);

  const cards: CardView[] = [];
  if (payload.items.length === 0) {
    const status = document.createElement("p");
    status.className = "status";
    status.textContent = "Nothing to switch to right now.";
    items.append(status);
  } else {
    for (const item of payload.items) {
      const card = createCard(item, payload.mode, payload.card_style);
      cards.push(card);
      items.append(card.element);
    }
  }

  section.append(caption, items);
  return {
    key: payloadKey(payload),
    style: payload.card_style,
    layout: payload.layout,
    generation: payload.generation,
    section,
    items,
    labelText,
    hintText,
    kbd,
    cards,
  };
}

function updateView(view: View, payload: SwitcherPayload): void {
  const label = captionFor(payload);
  if (view.labelText.data !== label) view.labelText.data = label;
  if (view.hintText.data !== payload.hint) view.hintText.data = payload.hint;
  view.kbd.hidden = payload.items.length === 0;
  view.section.setAttribute("aria-label", label);
  view.items.setAttribute("aria-label", label);
  view.generation = payload.generation;

  payload.items.forEach((item, index) => {
    const card = view.cards[index];
    if (card) setCardData(card, item, payload.mode);
  });
}

function revealSelected(container: HTMLElement, card: HTMLElement): void {
  if (
    container.scrollHeight <= container.clientHeight &&
    container.scrollWidth <= container.clientWidth
  ) {
    return;
  }
  const frame = container.getBoundingClientRect();
  const box = card.getBoundingClientRect();
  const top = box.top - frame.top + container.scrollTop;
  const bottom = top + box.height;
  const left = box.left - frame.left + container.scrollLeft;
  const right = left + box.width;

  if (top < container.scrollTop) {
    container.scrollTop = top;
  } else if (bottom > container.scrollTop + container.clientHeight) {
    container.scrollTop = bottom - container.clientHeight;
  }

  if (left < container.scrollLeft) {
    container.scrollLeft = left;
  } else if (right > container.scrollLeft + container.clientWidth) {
    container.scrollLeft = right - container.clientWidth;
  }
}

function markSelection(view: View, selected: number): void {
  view.cards.forEach((card, index) => {
    const isSelected = index === selected;
    card.element.classList.toggle("selected", isSelected);
    card.element.setAttribute("aria-selected", isSelected ? "true" : "false");
  });

  const active = view.cards[selected];
  if (active) {
    view.items.setAttribute("aria-activedescendant", active.element.id);
    revealSelected(view.items, active.element);
  } else {
    view.items.removeAttribute("aria-activedescendant");
  }
}

// ---------------------------------------------------------------------------
// Window previews
// ---------------------------------------------------------------------------

// Mirrors `.switcher.preview .item` in style.css and PREVIEW_CARD_PITCH /
// PREVIEW_ROW_HEIGHT in main.rs: a card is 240px wide and 46px of chrome plus
// a 132px preview tall at full size, with a 13px gap.
const PREVIEW_CARD_WIDTH = 240;
const PREVIEW_CARD_CHROME = 46;
const PREVIEW_SLOT_HEIGHT = 132;
const PREVIEW_GAP = 13;
const MIN_PREVIEW_SCALE = 0.3;

// Vertical preview rows (`.switcher.vertical.preview .item`, and
// VERTICAL_PREVIEW_* in main.rs): a 176x99 thumbnail beside a 210px label;
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
function verticalPreviewFits(scale: number, count: number, width: number, height: number): boolean {
  const rowHeight = VERTICAL_ROW_CHROME + VERTICAL_THUMB_HEIGHT * scale;
  const rowWidth = VERTICAL_ROW_CHROME + VERTICAL_ROW_LABEL + VERTICAL_THUMB_WIDTH * scale;
  const rows = Math.max(1, Math.floor((height + VERTICAL_ROW_GAP + 0.5) / (rowHeight + VERTICAL_ROW_GAP)));
  const columns = Math.ceil(count / rows);
  return columns * rowWidth + (columns - 1) * VERTICAL_COLUMN_GAP <= width + 0.5;
}

/** Shrinks the preview cards until every card fits without scrolling: a
 * live thumbnail cannot be clipped by an HTML scroll container. */
function fitPreviewCards(view: View): void {
  const style = getComputedStyle(view.items);
  const width =
    view.items.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  const height =
    view.items.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
  const count = view.cards.length;

  let scale = 1;
  for (; scale > MIN_PREVIEW_SCALE; scale -= 0.025) {
    if (view.layout === "vertical") {
      if (verticalPreviewFits(scale, count, width, height)) break;
      continue;
    }
    const cardWidth = PREVIEW_CARD_WIDTH * scale;
    const cardHeight = PREVIEW_CARD_CHROME + PREVIEW_SLOT_HEIGHT * scale;
    const columns = Math.max(1, Math.floor((width + PREVIEW_GAP) / (cardWidth + PREVIEW_GAP)));
    const rows = Math.ceil(count / columns);
    if (rows * cardHeight + (rows - 1) * PREVIEW_GAP <= height + 0.5) break;
  }
  view.section.style.setProperty("--card-scale", String(Math.max(scale, MIN_PREVIEW_SCALE)));
}

let lastThumbnailReport = "";
let thumbnailFrame = 0;

/** Tells the backend where each live preview goes on this overlay. Runs
 * after layout, so it is scheduled for the next animation frame. */
function scheduleThumbnailReport(): void {
  if (thumbnailFrame !== 0) return;
  thumbnailFrame = requestAnimationFrame(() => {
    thumbnailFrame = 0;
    const view = current;
    if (!view || view.style !== "preview") return;
    fitPreviewCards(view);

    const frame = view.items.getBoundingClientRect();
    const slots = [];
    for (const card of view.cards) {
      if (!card.thumbnail || !card.thumb) continue;
      const box = card.thumb.getBoundingClientRect();
      // A slot outside the visible list would draw over the panel chrome.
      const visible =
        box.width > 0 &&
        box.height > 0 &&
        box.top >= frame.top - 1 &&
        box.bottom <= frame.bottom + 1 &&
        box.left >= frame.left - 1 &&
        box.right <= frame.right + 1;
      if (!visible) continue;
      slots.push({ id: card.id, x: box.left, y: box.top, width: box.width, height: box.height });
    }
    const request = {
      generation: view.generation,
      scale: window.devicePixelRatio || 1,
      slots,
    };
    const key = JSON.stringify(request);
    if (key === lastThumbnailReport) return;
    lastThumbnailReport = key;
    invoke("update_thumbnails", request).catch((error: unknown) => {
      lastThumbnailReport = "";
      console.warn("Unable to place window previews", error);
    });
  });
}

window.addEventListener("resize", () => {
  // The backend resizes the overlay after emitting the payload, and moving
  // between monitors changes the device pixel ratio.
  lastThumbnailReport = "";
  scheduleThumbnailReport();
});

let renderedGeneration: number | null = null;
let openingFrame = 0;

function render(payload: SwitcherPayload): void {
  renderPayload(payload);
  if (payload.generation !== renderedGeneration) {
    renderedGeneration = payload.generation;
    markOpening();
  }
  if (payload.card_style === "preview") scheduleThumbnailReport();
}

/** A new opening: paint the selection without its transition, then allow it
 * again for the selection changes that follow. */
function markOpening(): void {
  const section = current?.section;
  if (!section) return;
  section.classList.add("opening");
  cancelAnimationFrame(openingFrame);
  // Two frames: the first runs before the opening paints, the second after.
  openingFrame = requestAnimationFrame(() => {
    openingFrame = requestAnimationFrame(() => {
      openingFrame = 0;
      section.classList.remove("opening");
    });
  });
}

function renderPayload(payload: SwitcherPayload): void {
  const stale = current;
  if (stale && stale.key === payloadKey(payload)) {
    updateView(stale, payload);
    markSelection(stale, payload.selected);
    return;
  }

  const next = buildView(payload);
  const scrollTop = stale ? stale.items.scrollTop : 0;
  const scrollLeft = stale ? stale.items.scrollLeft : 0;

  current = next;
  app.replaceChildren(next.section);

  if (next.items.scrollHeight > next.items.clientHeight) {
    next.items.scrollTop = scrollTop;
  }
  if (next.items.scrollWidth > next.items.clientWidth) {
    next.items.scrollLeft = scrollLeft;
  }

  markSelection(next, payload.selected);
}

function renderStatus(label: string, message: string): void {
  const section = document.createElement("section");
  section.className = "switcher status-panel";
  section.setAttribute("role", "alert");
  section.setAttribute("aria-label", label);

  const caption = document.createElement("div");
  caption.className = "caption";
  const captionLabel = document.createElement("span");
  captionLabel.textContent = label;
  caption.append(captionLabel);

  const text = document.createElement("p");
  text.className = "status";
  text.textContent = message;

  section.append(caption, text);
  current = null;
  app.replaceChildren(section);
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

function applyPayload(value: unknown): void {
  const arrived = performance.now();
  const payload = parsePayload(value);
  if (!payload) {
    console.warn("Ignoring malformed switcher state", value);
    return;
  }
  receivedState = true;
  timingEnabled = payload.timing;
  pendingIconDecodes.length = 0;
  const opening = payload.generation !== renderedGeneration;
  if (payload.timing && payload.sent_at !== null) {
    invoke("overlay_timing", {
      generation: payload.generation,
      stage: opening ? "delivered (open)" : "delivered (update)",
      webviewMs: performance.timeOrigin + arrived - payload.sent_at,
    }).catch(() => undefined);
  }
  render(payload);
  if (payload.timing && opening) reportOpeningTimes(payload.generation, arrived);
}

/** Latency logging only (see timing.rs): tells the backend when this opening
 * was rendered, painted, and had every icon decoded. */
function reportOpeningTimes(generation: number, arrived: number): void {
  const report = (stage: string): void => {
    invoke("overlay_timing", {
      generation,
      stage,
      webviewMs: performance.now() - arrived,
    }).catch(() => undefined);
  };
  report("rendered");
  const decodes = pendingIconDecodes.splice(0);
  requestAnimationFrame(() => {
    report("painted");
    void Promise.allSettled(decodes).then(() => report(`icons drawn (${decodes.length})`));
  });
}

type SelectionPayload = { generation: number; selected: number; sent_at: number | null };

function parseSelection(value: unknown): SelectionPayload | null {
  if (!isRecord(value)) return null;
  const { generation, selected } = value;
  if (typeof generation !== "number" || !Number.isFinite(generation)) return null;
  if (typeof selected !== "number" || !Number.isInteger(selected)) return null;
  const sent_at = typeof value.sent_at === "number" ? value.sent_at : null;
  return { generation, selected, sent_at };
}

/** The backend only moved the selection within the opening on screen. */
function applySelection(value: unknown): void {
  const arrived = performance.now();
  const selection = parseSelection(value);
  const view = current;
  if (!selection || !view || view.generation !== selection.generation) return;
  if (selection.selected < 0 || selection.selected >= view.cards.length) return;
  if (timingEnabled && selection.sent_at !== null) {
    invoke("overlay_timing", {
      generation: selection.generation,
      stage: "delivered (select)",
      webviewMs: performance.timeOrigin + arrived - selection.sent_at,
    }).catch(() => undefined);
  }
  markSelection(view, selection.selected);
  if (view.style === "preview") scheduleThumbnailReport();
}

async function start(): Promise<void> {
  let stop: UnlistenFn;
  try {
    const stopSelect = await listen<unknown>("switcher:select", ({ payload }) => {
      try {
        applySelection(payload);
      } catch (error) {
        console.error("Unable to move the switcher selection", error);
      }
    });
    // Register before asking for a snapshot: the first event can be emitted
    // while the WebView is still loading.
    const stopState = await listen<unknown>("switcher:state", ({ payload }) => {
      try {
        applyPayload(payload);
      } catch (error) {
        // Never let a rendering bug take down the listener.
        console.error("Unable to render switcher state", error);
      }
    });
    stop = () => {
      stopSelect();
      stopState();
    };
  } catch (error) {
    console.error("Unable to listen for switcher updates", error);
    renderStatus("Window Switcher", "The switcher service is not reachable.");
    return;
  }

  // Hot reload may have disposed the module while the listener registered.
  if (disposed) {
    void stop();
    return;
  }
  unlisten = stop;

  try {
    const snapshot = await invoke<unknown>("switcher_snapshot");
    // If a live event already arrived, its state is newer than the snapshot.
    if (snapshot !== null && !receivedState && !disposed) applyPayload(snapshot);
  } catch (error) {
    // Older backends may not expose the command; live events still work.
    console.warn("Unable to load the initial switcher state", error);
  }
}

void start();

type HotModule = {
  dispose: (callback: () => void) => void;
};

const hot = (import.meta as ImportMeta & { hot?: HotModule }).hot;
hot?.dispose(() => {
  disposed = true;
  if (unlisten) {
    void unlisten();
    unlisten = null;
  }
});
