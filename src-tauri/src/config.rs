//! `config.json`: the settings file, read once at startup and updated one
//! key at a time by the Settings window.

use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    lock_unpoisoned,
    settings_model::{CardStyle, Layout, MonitorMode},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct AppConfig {
    pub(crate) window_hotkey: String,
    pub(crate) app_hotkey: String,
    /// "primary", "every" or "cursor"; anything else falls back to "primary".
    pub(crate) show_switcher_on: String,
    /// "icon" or "preview"; anything else falls back to "icon".
    pub(crate) card_style: String,
    /// "horizontal" or "vertical"; anything else falls back to "horizontal".
    pub(crate) layout: String,
    /// Offer the Settings window as a switch target, and switch from it like
    /// from any other window, instead of skipping it.
    pub(crate) settings_in_switcher: bool,
    /// Restart elevated (through the UAC prompt) when started without
    /// administrator rights; see `elevation.rs`.
    pub(crate) run_as_admin: bool,
    /// Check GitHub for a new release at startup and every 6 hours.
    pub(crate) check_for_updates: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // The backtick shortcut also accepts the physical French "²" key:
            // both are scancode 0x29, so the scan-code match covers each layout.
            window_hotkey: "Alt+Backtick".into(),
            app_hotkey: "Win+Tab".into(),
            show_switcher_on: MonitorMode::Primary.as_str().into(),
            card_style: CardStyle::Icon.as_str().into(),
            layout: Layout::Horizontal.as_str().into(),
            settings_in_switcher: false,
            run_as_admin: false,
            check_for_updates: true,
        }
    }
}

pub(crate) fn app_data_path() -> PathBuf {
    let root = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join("Window Switcher")
}

/// Serializes every read-modify-write of `config.json`. Setting commands are
/// async, so two quick clicks can run at the same time; whoever holds this
/// also updates the live setting, so the file and memory never disagree.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Proof that the caller holds `CONFIG_LOCK`.
type ConfigLock = MutexGuard<'static, ()>;

pub(crate) fn lock_config() -> ConfigLock {
    lock_unpoisoned(&CONFIG_LOCK)
}

/// Saves `values`, then makes them live with `apply`, under the config lock:
/// a failed save changes nothing, and two quick changes can never leave
/// memory and the file holding different values.
pub(crate) fn persist(
    values: &[(&str, serde_json::Value)],
    apply: impl FnOnce(),
) -> Result<(), String> {
    let lock = lock_config();
    save_config_values(&lock, values)?;
    apply();
    Ok(())
}

fn config_file(directory: &Path) -> PathBuf {
    directory.join("config.json")
}

/// Writes `config.json` through a temporary file and a rename, so a crash or
/// power loss mid-write leaves either the old or the new file, never half of one.
fn write_config_file(
    _lock: &ConfigLock,
    directory: &Path,
    root: &serde_json::Value,
) -> Result<(), String> {
    let file = config_file(directory);
    let temporary = directory.join("config.json.tmp");
    let contents = serde_json::to_string_pretty(root).map_err(|error| error.to_string())?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("unable to create {}: {error}", directory.display()))?;
    fs::write(&temporary, contents)
        .map_err(|error| format!("unable to write {}: {error}", temporary.display()))?;
    // On Windows `rename` replaces an existing file (MOVEFILE_REPLACE_EXISTING).
    fs::rename(&temporary, &file).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("unable to replace {}: {error}", file.display())
    })
}

/// Takes each known setting from the file when it has the default's JSON
/// type; a missing or wrong-typed one keeps its default, so one bad value
/// never resets the others. Driven by `AppConfig` itself, so a new field
/// needs no extra code here.
fn config_from_object(object: &serde_json::Map<String, serde_json::Value>) -> AppConfig {
    let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(AppConfig::default())
    else {
        return AppConfig::default();
    };
    for (key, default) in merged.iter_mut() {
        match object.get(key) {
            Some(value) if std::mem::discriminant(value) == std::mem::discriminant(default) => {
                *default = value.clone();
            }
            Some(value) => {
                log::warn!("config.json: {key} has the wrong type ({value}); using {default}")
            }
            None => {}
        }
    }
    serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default()
}

pub(crate) fn load_config() -> AppConfig {
    let lock = lock_config();
    load_config_in(&lock, &app_data_path())
}

fn load_config_in(lock: &ConfigLock, directory: &Path) -> AppConfig {
    let file = config_file(directory);

    let contents = match fs::read_to_string(&file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let config = AppConfig::default();
            match serde_json::to_value(&config) {
                Ok(root) => {
                    if let Err(error) = write_config_file(lock, directory, &root) {
                        log::warn!("{error}");
                    }
                }
                Err(error) => log::warn!("unable to serialize the default config: {error}"),
            }
            return config;
        }
        Err(error) => {
            log::error!("unable to read {}: {error}; using defaults", file.display());
            return AppConfig::default();
        }
    };

    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(serde_json::Value::Object(object)) => config_from_object(&object),
        parsed => {
            // Never overwrite the file: keep the user's text for inspection and
            // start from defaults so the hotkeys still work. The timestamp
            // keeps an earlier backup from being replaced.
            let reason = match parsed {
                Err(error) => error.to_string(),
                Ok(_) => "the top level is not an object".to_string(),
            };
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            let invalid = directory.join(format!("config.invalid-{stamp}.json"));
            log::error!(
                "{} is not a valid config ({reason}); moving it to {}",
                file.display(),
                invalid.display()
            );
            if let Err(rename_error) = fs::rename(&file, &invalid) {
                log::error!("unable to move the invalid config aside: {rename_error}");
            }
            AppConfig::default()
        }
    }
}

/// Writes keys into `config.json`, keeping the rest of the file as the user
/// left it (including keys this version does not know). All values land in
/// one write. A file that exists but cannot be parsed is left untouched and
/// the save fails, rather than replacing the user's text with defaults.
pub(crate) fn save_config_values(
    lock: &ConfigLock,
    values: &[(&str, serde_json::Value)],
) -> Result<(), String> {
    save_config_values_in(lock, &app_data_path(), values)
}

fn save_config_values_in(
    lock: &ConfigLock,
    directory: &Path,
    values: &[(&str, serde_json::Value)],
) -> Result<(), String> {
    let file = config_file(directory);
    let mut root = match fs::read_to_string(&file) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) if value.is_object() => value,
            _ => {
                return Err(format!(
                    "{} is not valid JSON; fix or delete it, then try again",
                    file.display()
                ))
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_value(AppConfig::default()).map_err(|error| error.to_string())?
        }
        Err(error) => return Err(format!("unable to read {}: {error}", file.display())),
    };
    if let Some(object) = root.as_object_mut() {
        for (key, value) in values {
            object.insert(key.to_string(), value.clone());
        }
    }
    write_config_file(lock, directory, &root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `config.json` as this version writes it for a new user. Keys are
    /// sorted (serde_json's default map) and indented by two spaces.
    const GOLDEN_DEFAULT: &str = r#"{
  "app_hotkey": "Win+Tab",
  "card_style": "icon",
  "check_for_updates": true,
  "layout": "horizontal",
  "run_as_admin": false,
  "settings_in_switcher": false,
  "show_switcher_on": "primary",
  "window_hotkey": "Alt+Backtick"
}"#;

    /// A fresh, empty directory under the system temp folder.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "window-switcher-test-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, contents: &str) {
            fs::write(config_file(&self.0), contents).unwrap();
        }

        fn read(&self) -> String {
            fs::read_to_string(config_file(&self.0)).unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn load(dir: &TempDir) -> AppConfig {
        load_config_in(&lock_config(), &dir.0)
    }

    fn object(json: &str) -> AppConfig {
        match serde_json::from_str(json).unwrap() {
            serde_json::Value::Object(object) => config_from_object(&object),
            _ => panic!("not an object"),
        }
    }

    fn assert_config(config: &AppConfig, expected: [&str; 5]) {
        let actual = [
            config.window_hotkey.as_str(),
            config.app_hotkey.as_str(),
            config.show_switcher_on.as_str(),
            config.card_style.as_str(),
            config.layout.as_str(),
        ];
        assert_eq!(actual, expected);
    }

    const DEFAULTS: [&str; 5] = ["Alt+Backtick", "Win+Tab", "primary", "icon", "horizontal"];

    #[test]
    fn the_default_config_serializes_to_the_golden_file() {
        let root = serde_json::to_value(AppConfig::default()).unwrap();
        assert_eq!(serde_json::to_string_pretty(&root).unwrap(), GOLDEN_DEFAULT);
    }

    #[test]
    fn a_missing_file_is_created_with_the_defaults() {
        let dir = TempDir::new("missing");
        assert_config(&load(&dir), DEFAULTS);
        assert_eq!(dir.read(), GOLDEN_DEFAULT);
        assert!(!dir.0.join("config.json.tmp").exists());
    }

    #[test]
    fn reads_an_existing_file_in_the_on_disk_format() {
        let dir = TempDir::new("existing");
        dir.write(
            r#"{
  "app_hotkey": "Alt+Backtick",
  "card_style": "preview",
  "layout": "vertical",
  "show_switcher_on": "cursor",
  "window_hotkey": "Win+Tab"
}"#,
        );
        assert_config(
            &load(&dir),
            ["Win+Tab", "Alt+Backtick", "cursor", "preview", "vertical"],
        );
    }

    #[test]
    fn missing_fields_keep_their_defaults() {
        assert_config(&object("{}"), DEFAULTS);
        assert_config(
            &object(r#"{"layout": "vertical"}"#),
            ["Alt+Backtick", "Win+Tab", "primary", "icon", "vertical"],
        );
    }

    #[test]
    fn a_wrong_typed_field_keeps_its_default_and_the_others_survive() {
        let config = object(
            r#"{"card_style": 3, "layout": null, "app_hotkey": ["Win+Tab"],
                "show_switcher_on": "every", "window_hotkey": "Win+Backtick"}"#,
        );
        assert_config(
            &config,
            ["Win+Backtick", "Win+Tab", "every", "icon", "horizontal"],
        );
    }

    #[test]
    fn unknown_values_are_kept_for_the_caller_to_validate() {
        // The string type is right, so it is loaded as is; startup falls
        // back per setting (see `MonitorMode::parse` and friends).
        let config = object(r#"{"show_switcher_on": "nowhere"}"#);
        assert_eq!(config.show_switcher_on, "nowhere");
    }

    #[test]
    fn unknown_keys_are_ignored_on_load_and_kept_on_save() {
        let dir = TempDir::new("unknown");
        dir.write(r#"{"future_setting": {"a": 1}, "layout": "vertical"}"#);
        assert_config(
            &load(&dir),
            ["Alt+Backtick", "Win+Tab", "primary", "icon", "vertical"],
        );
        save_config_values_in(&lock_config(), &dir.0, &[("card_style", "preview".into())]).unwrap();
        assert_eq!(
            dir.read(),
            r#"{
  "card_style": "preview",
  "future_setting": {
    "a": 1
  },
  "layout": "vertical"
}"#
        );
    }

    #[test]
    fn saving_without_a_file_starts_from_the_defaults() {
        let dir = TempDir::new("save-new");
        let _ = fs::remove_file(config_file(&dir.0));
        save_config_values_in(
            &lock_config(),
            &dir.0,
            &[
                ("window_hotkey", "Win+Tab".into()),
                ("app_hotkey", "Alt+Backtick".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            dir.read(),
            GOLDEN_DEFAULT
                .replace("\"Win+Tab\"", "\"SWAP\"")
                .replace("\"Alt+Backtick\"", "\"Win+Tab\"")
                .replace("\"SWAP\"", "\"Alt+Backtick\"")
        );
    }

    #[test]
    fn an_invalid_file_is_moved_aside_and_never_overwritten() {
        let dir = TempDir::new("invalid");
        dir.write("{ not json");
        assert!(
            save_config_values_in(&lock_config(), &dir.0, &[("layout", "vertical".into())])
                .is_err()
        );
        assert_eq!(dir.read(), "{ not json");

        assert_config(&load(&dir), DEFAULTS);
        assert!(!config_file(&dir.0).exists());
        let moved: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].starts_with("config.invalid-") && moved[0].ends_with(".json"));
        assert_eq!(
            fs::read_to_string(dir.0.join(&moved[0])).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn a_top_level_array_counts_as_invalid() {
        let dir = TempDir::new("array");
        dir.write("[1, 2]");
        assert_config(&load(&dir), DEFAULTS);
        assert!(!config_file(&dir.0).exists());
    }
}
