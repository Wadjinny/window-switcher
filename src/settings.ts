import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./settings.css";

// The Settings window. Every setting is keyed like `config.json` and saved
// through Rust's generic `set_setting(key, value)`, which returns all settings.
//
// To add a choice setting: add its allowed values to CHOICES, add the radio
// fieldset (inputs named after the key) to settings.html, and add a match arm
// to `set_setting` in ipc.rs. On/off settings are checkboxes named after a
// key in TOGGLE_KEYS; they are sent as "true"/"false" and come back as
// booleans. Other kinds of controls register a `render` function in
// RENDERERS so they refresh whenever settings are loaded or saved.

const CHOICES = {
  show_switcher_on: ["primary", "every", "cursor"],
  card_style: ["icon", "preview"],
  layout: ["horizontal", "vertical"],
} as const;

const HOTKEY_KEYS = ["app_hotkey", "window_hotkey"] as const;

const TOGGLE_KEYS = [
  "settings_in_switcher",
  "start_with_windows",
  "run_as_admin",
  "check_for_updates",
] as const;

type ChoiceKey = keyof typeof CHOICES;
type HotkeyKey = (typeof HOTKEY_KEYS)[number];
type ToggleKey = (typeof TOGGLE_KEYS)[number];
type Settings = Partial<
  Record<ChoiceKey | HotkeyKey, string> &
    Record<ToggleKey, boolean> & {
      /** Read-only: whether the app is elevated right now. */
      running_as_admin: boolean;
    }
>;

const CHOICE_KEYS = Object.keys(CHOICES) as ChoiceKey[];
const DEFAULT_STATUS = "Changes apply and save immediately.";

const status = document.querySelector<HTMLElement>("#status")!;
const hotkeyLive = document.querySelector<HTMLElement>("#hotkey-live")!;

let current: Settings = {};

function isChoiceKey(value: string): value is ChoiceKey {
  return (CHOICE_KEYS as string[]).includes(value);
}

function isHotkeyKey(value: string | undefined): value is HotkeyKey {
  return (HOTKEY_KEYS as readonly (string | undefined)[]).includes(value);
}

function isToggleKey(value: string): value is ToggleKey {
  return (TOGGLE_KEYS as readonly string[]).includes(value);
}

function isChoice<K extends ChoiceKey>(
  key: K,
  value: unknown,
): value is (typeof CHOICES)[K][number] {
  return typeof value === "string" && (CHOICES[key] as readonly string[]).includes(value);
}

function parseSettings(value: unknown): Settings {
  const settings: Settings = {};
  if (typeof value !== "object" || value === null) return settings;
  const record = value as Record<string, unknown>;
  for (const key of CHOICE_KEYS) {
    const choice = record[key];
    if (isChoice(key, choice)) settings[key] = choice;
  }
  for (const key of HOTKEY_KEYS) {
    const hotkey = record[key];
    if (typeof hotkey === "string" && parseHotkey(hotkey)) settings[key] = hotkey;
  }
  for (const key of TOGGLE_KEYS) {
    const enabled = record[key];
    if (typeof enabled === "boolean") settings[key] = enabled;
  }
  if (typeof record.running_as_admin === "boolean") {
    settings.running_as_admin = record.running_as_admin;
  }
  return settings;
}

function setStatus(message: string, isError = false): void {
  status.textContent = message;
  status.classList.toggle("error", isError);
}

// ---------------------------------------------------------------------------
// Choice settings (radio groups)
// ---------------------------------------------------------------------------

const radios = Array.from(document.querySelectorAll<HTMLInputElement>("input[type=radio]"));

function renderChoices(settings: Settings): void {
  for (const radio of radios) {
    if (!isChoiceKey(radio.name)) continue;
    const value = settings[radio.name];
    if (value !== undefined) radio.checked = radio.value === value;
  }
}

for (const radio of radios) {
  radio.addEventListener("change", () => {
    const key = radio.name;
    if (radio.checked && isChoiceKey(key) && isChoice(key, radio.value)) {
      void save(() => invoke<unknown>("set_setting", { key, value: radio.value }));
    }
  });
}

// ---------------------------------------------------------------------------
// On/off settings (checkboxes)
// ---------------------------------------------------------------------------

const checkboxes = Array.from(
  document.querySelectorAll<HTMLInputElement>("input[type=checkbox]"),
);

function renderToggles(settings: Settings): void {
  for (const checkbox of checkboxes) {
    if (!isToggleKey(checkbox.name)) continue;
    const value = settings[checkbox.name];
    if (value !== undefined) checkbox.checked = value;
  }
}

for (const checkbox of checkboxes) {
  checkbox.addEventListener("change", () => {
    const key = checkbox.name;
    if (isToggleKey(key)) {
      const value = String(checkbox.checked);
      if (key === "run_as_admin" && checkbox.checked && current.running_as_admin === false) {
        // On success the app restarts elevated and this window closes; the
        // reply never comes.
        setStatus("Restarting as administrator: confirm the Windows prompt.");
      }
      void save(() => invoke<unknown>("set_setting", { key, value }));
    }
  });
}

const adminStatus = document.getElementById("admin-status");

function renderAdminStatus(settings: Settings): void {
  if (!adminStatus || settings.running_as_admin === undefined) return;
  if (!settings.running_as_admin) {
    adminStatus.textContent = "Currently running without administrator rights.";
  } else if (settings.run_as_admin === false) {
    adminStatus.textContent = "Currently running as administrator, until Window Switcher restarts.";
  } else {
    adminStatus.textContent = "Currently running as administrator.";
  }
}

// ---------------------------------------------------------------------------
// Hotkeys
// ---------------------------------------------------------------------------
//
// The hook format (see `Shortcut::parse` in main.rs) is exactly one modifier,
// Alt or Win, plus Tab or Backtick, written like "Alt+Backtick". Shift is not
// a usable modifier: holding it reverses the selection while switching.
//
// While a field records, Rust swallows key presses in the Settings window and
// sends them as `settings:hotkey-recorded`, so the current hotkeys and
// Windows' own Win+Tab / Alt+Tab never fire in the middle of recording.

const HOTKEY_MODIFIERS = ["Alt", "Win"] as const;
const HOTKEY_KEYS_ALLOWED = ["Tab", "Backtick"] as const;
type HotkeyModifier = (typeof HOTKEY_MODIFIERS)[number];
type HotkeyKeyName = (typeof HOTKEY_KEYS_ALLOWED)[number];

const HOTKEY_LABELS: Record<HotkeyKey, string> = {
  app_hotkey: "Switch apps",
  window_hotkey: "Switch windows of current app",
};

type RecordedHotkey = {
  alt: boolean;
  win: boolean;
  ctrl: boolean;
  shift: boolean;
  escape: boolean;
  key: string | null;
  key_name: string;
};

const hotkeyFields = new Map<HotkeyKey, HTMLButtonElement>();
for (const button of document.querySelectorAll<HTMLButtonElement>("button[data-hotkey]")) {
  const key = button.dataset.hotkey;
  if (isHotkeyKey(key)) hotkeyFields.set(key, button);
}
const resetButton = document.querySelector<HTMLButtonElement>("#reset-hotkeys")!;

let recording: HotkeyKey | null = null;

/** Accepts the canonical form Rust stores, e.g. "Win+Tab". */
function parseHotkey(value: string): { modifier: HotkeyModifier; key: HotkeyKeyName } | null {
  const [modifier, key, ...rest] = value.split("+");
  if (rest.length > 0) return null;
  const mod = HOTKEY_MODIFIERS.find((name) => name === modifier);
  const k = HOTKEY_KEYS_ALLOWED.find((name) => name === key);
  return mod && k ? { modifier: mod, key: k } : null;
}

/** "Alt+Backtick" -> "Alt + `", matching the overlay's hint. */
function formatHotkey(value: string | undefined): string {
  const parsed = value ? parseHotkey(value) : null;
  if (!parsed) return "—";
  return `${parsed.modifier} + ${parsed.key === "Backtick" ? "`" : parsed.key}`;
}

function otherHotkey(key: HotkeyKey): HotkeyKey {
  return key === "app_hotkey" ? "window_hotkey" : "app_hotkey";
}

/** Turns a recorded combination into a config value, or explains why not. */
function validateRecorded(
  field: HotkeyKey,
  combo: RecordedHotkey,
): { value: string } | { error: string } {
  const modifiers = [combo.alt, combo.win, combo.ctrl, combo.shift].filter(Boolean).length;
  const keyLabel = combo.key === "Backtick" ? "`" : combo.key ?? combo.key_name;
  if (modifiers === 0) {
    return { error: `Hold Alt or Win while pressing ${keyLabel}.` };
  }
  if (combo.ctrl) return { error: "Ctrl shortcuts aren't supported. Use Alt or Win." };
  if (combo.shift) {
    return { error: "Shift is reserved for going backwards. Use Alt or Win alone." };
  }
  if (combo.alt && combo.win) return { error: "Use only one modifier: Alt or Win." };
  if (combo.key === null || !HOTKEY_KEYS_ALLOWED.some((name) => name === combo.key)) {
    return { error: `${combo.key_name} can't be used. Press Tab or \` (the key above Tab).` };
  }
  // The other switcher's shortcut is fine: Rust swaps the two.
  return { value: `${combo.alt ? "Alt" : "Win"}+${combo.key}` };
}

function setHotkeyError(key: HotkeyKey, message: string): void {
  const error = document.getElementById(`${key}-error`);
  if (error) error.textContent = message;
  hotkeyFields.get(key)?.classList.toggle("invalid", message.length > 0);
}

function renderHotkeys(settings: Settings): void {
  for (const [key, button] of hotkeyFields) {
    const isRecording = recording === key;
    button.classList.toggle("recording", isRecording);
    button.textContent = isRecording ? "Press a shortcut…" : formatHotkey(settings[key]);
  }
}

function setRecording(key: HotkeyKey | null): void {
  if (recording === key) return;
  const previous = recording;
  if (previous) setHotkeyError(previous, "");
  recording = key;
  renderHotkeys(current);
  // Announced through a live region: a toggle state on the button would not
  // say what to do next.
  if (key) {
    hotkeyLive.textContent = `Recording a shortcut for ${HOTKEY_LABELS[key]}. Hold Alt or Win and press Tab or the key above Tab. Escape cancels.`;
  } else if (previous) {
    hotkeyLive.textContent = `Stopped recording. ${HOTKEY_LABELS[previous]} is ${formatHotkey(current[previous])}.`;
  }
  invoke("set_hotkey_recording", { active: key !== null }).catch((error: unknown) => {
    console.error("Unable to toggle hotkey recording", error);
    recording = null;
    renderHotkeys(current);
    setStatus(`Could not record a shortcut: ${String(error)}`, true);
  });
}

function onRecorded(combo: RecordedHotkey): void {
  const field = recording;
  if (!field) return;
  const bare = !combo.alt && !combo.win && !combo.ctrl && !combo.shift;
  if (combo.escape && bare) {
    setRecording(null);
    return;
  }
  const result = validateRecorded(field, combo);
  if ("error" in result) {
    // Keep recording so the user can try again right away.
    setHotkeyError(field, result.error);
    return;
  }
  setRecording(null);
  if (current[field] === result.value) return;
  const other = otherHotkey(field);
  void save(() => invoke<unknown>("set_setting", { key: field, value: result.value }), field).then(
    (before) => {
      // Rust swaps the shortcuts when the other switcher already used it.
      if (before && before[other] === result.value && current[other] !== result.value) {
        setStatus(`Swapped: “${HOTKEY_LABELS[other]}” is now ${formatHotkey(current[other])}.`);
      }
    },
  );
}

function isRecordedHotkey(value: unknown): value is RecordedHotkey {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.alt === "boolean" &&
    typeof v.win === "boolean" &&
    typeof v.ctrl === "boolean" &&
    typeof v.shift === "boolean" &&
    typeof v.escape === "boolean" &&
    (v.key === null || typeof v.key === "string") &&
    typeof v.key_name === "string"
  );
}

for (const [key, button] of hotkeyFields) {
  button.addEventListener("click", () => setRecording(recording === key ? null : key));
}

resetButton.addEventListener("click", () => {
  setRecording(null);
  for (const key of HOTKEY_KEYS) setHotkeyError(key, "");
  void save(() => invoke<unknown>("reset_hotkeys"));
});

// Recording only makes sense while this window has focus (Rust also stops
// it when the window loses focus); clicking anywhere else cancels it.
window.addEventListener("blur", () => setRecording(null));
document.addEventListener("pointerdown", (event) => {
  if (recording && !(event.target as Element | null)?.closest("button[data-hotkey]")) {
    setRecording(null);
  }
});

void listen<unknown>("settings:hotkey-recorded", ({ payload }) => {
  if (isRecordedHotkey(payload)) onRecorded(payload);
}).catch((error: unknown) => {
  console.error("Unable to listen for recorded hotkeys", error);
});

// ---------------------------------------------------------------------------
// Updates (see update/mod.rs). Not a setting: its own commands, and status
// events while a check or an install runs.
// ---------------------------------------------------------------------------

type UpdateStatus = {
  current: string;
  available: string | null;
  state: "idle" | "checking" | "up_to_date" | "available" | "installing" | "failed";
  checked_at?: number;
  version?: string;
  message?: string;
};

const UPDATE_STATES = ["idle", "checking", "up_to_date", "available", "installing", "failed"];

function parseUpdateStatus(value: unknown): UpdateStatus | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.current !== "string" || typeof v.state !== "string") return null;
  if (!UPDATE_STATES.includes(v.state)) return null;
  return {
    current: v.current,
    available: typeof v.available === "string" ? v.available : null,
    state: v.state as UpdateStatus["state"],
    checked_at: typeof v.checked_at === "number" ? v.checked_at : undefined,
    version: typeof v.version === "string" ? v.version : undefined,
    message: typeof v.message === "string" ? v.message : undefined,
  };
}

const updateTitle = document.querySelector<HTMLElement>("#update-title")!;
const updateDetail = document.querySelector<HTMLElement>("#update-detail")!;
const updateAction = document.querySelector<HTMLButtonElement>("#update-action")!;
const releaseNotes = document.querySelector<HTMLButtonElement>("#release-notes")!;
let update: UpdateStatus | null = null;

function timeAgo(unixSeconds: number): string {
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - unixSeconds) / 60));
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  return `${hours} h ago`;
}

function renderUpdate(status: UpdateStatus): void {
  update = status;
  updateTitle.textContent = `Window Switcher ${status.current}`;
  let detail = "";
  let action = "Check now";
  let busy = false;
  switch (status.state) {
    case "idle":
      detail = "Not checked yet.";
      break;
    case "checking":
      detail = "Checking for updates…";
      action = "Checking…";
      busy = true;
      break;
    case "up_to_date":
      detail = `Up to date. Checked ${timeAgo(status.checked_at ?? 0)}.`;
      break;
    case "available":
      detail = `Version ${status.version} is available.`;
      break;
    case "installing":
      detail = `Downloading ${status.version}; Window Switcher restarts when it is ready.`;
      action = "Updating…";
      busy = true;
      break;
    case "failed":
      detail = status.message ?? "Something went wrong.";
      break;
  }
  if (!busy && status.available) action = `Update to ${status.available}`;
  updateDetail.textContent = detail;
  updateDetail.classList.toggle("error", status.state === "failed");
  updateAction.textContent = action;
  updateAction.disabled = busy;
  updateAction.classList.toggle("primary", !busy && status.available !== null);
  releaseNotes.hidden = status.available === null;
}

async function loadUpdate(): Promise<void> {
  try {
    const status = parseUpdateStatus(await invoke<unknown>("get_update_status"));
    if (status) renderUpdate(status);
  } catch (error) {
    console.error("Unable to read the update status", error);
  }
}

updateAction.addEventListener("click", () => {
  if (update?.available && update.state !== "installing") {
    void invoke("install_update").catch((error: unknown) => {
      setStatus(`Could not update: ${String(error)}`, true);
    });
    return;
  }
  void invoke<unknown>("check_for_updates")
    .then((value) => {
      const status = parseUpdateStatus(value);
      if (status) renderUpdate(status);
    })
    .catch((error: unknown) => setStatus(`Could not check: ${String(error)}`, true));
});

releaseNotes.addEventListener("click", () => {
  void invoke("open_release_page").catch(() => {});
});

void listen<unknown>("update:status", ({ payload }) => {
  const status = parseUpdateStatus(payload);
  if (status) renderUpdate(status);
}).catch((error: unknown) => {
  console.error("Unable to listen for update status", error);
});

// ---------------------------------------------------------------------------
// Load and save
// ---------------------------------------------------------------------------

const RENDERERS: ((settings: Settings) => void)[] = [
  renderChoices,
  renderToggles,
  renderAdminStatus,
  renderHotkeys,
];

function render(settings: Settings): void {
  current = { ...current, ...settings };
  for (const renderer of RENDERERS) renderer(current);
}

// Saves run one at a time, in the order they were made. Controls stay
// enabled (disabling them would drop keyboard focus); a change made while an
// earlier one is saving simply waits its turn.
let saving: Promise<unknown> = Promise.resolve();

/**
 * Queues a settings command; inline errors go to `field` when it is a hotkey.
 * Resolves with the settings as they were just before this command ran, or
 * null if it failed.
 */
function save(command: () => Promise<unknown>, field?: HotkeyKey): Promise<Settings | null> {
  const run = saving.then(() => runSave(command, field));
  saving = run;
  return run;
}

async function runSave(
  command: () => Promise<unknown>,
  field?: HotkeyKey,
): Promise<Settings | null> {
  const before = { ...current };
  try {
    render(parseSettings(await command()));
    if (field) setHotkeyError(field, "");
    setStatus("Saved.");
    return before;
  } catch (error) {
    console.error("Unable to save the setting", error);
    if (field) {
      setHotkeyError(field, `Not saved: ${String(error)}`);
      setStatus(DEFAULT_STATUS);
    } else {
      setStatus(`Could not save: ${String(error)}`, true);
    }
    await load();
    return null;
  }
}

async function load(): Promise<void> {
  try {
    render(parseSettings(await invoke<unknown>("get_settings")));
  } catch (error) {
    console.error("Unable to load settings", error);
    setStatus("Settings could not be loaded.", true);
  }
}

setStatus(DEFAULT_STATUS);
renderHotkeys(current);
// A previous Settings window may have closed mid-recording; start clean.
void invoke("set_hotkey_recording", { active: false }).catch(() => {});
void load();
void loadUpdate();
