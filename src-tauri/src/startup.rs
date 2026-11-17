//! "Start with Windows": launching this executable when the user signs in.
//!
//! Two mechanisms, never both at once:
//!
//! - A per-user `Run` registry value. No admin rights are needed, but
//!   Windows starts `Run` entries without administrator rights, whatever the
//!   program asks for.
//! - With "Run as administrator" on, a scheduled task in the root Task
//!   Scheduler folder with a logon trigger for this user and "Run with
//!   highest privileges", so the app starts elevated without a UAC prompt.
//!   Only an elevated process can register such a task, so while the app
//!   runs without administrator rights it keeps (or writes) the `Run` value
//!   instead, and the next elevated launch swaps it for the task (see
//!   [`reconcile`]). At sign-in that `Run` entry starts the app normally and
//!   it asks for elevation itself (see `elevation.rs`).
//!
//! Neither lives in `config.json`: the toggle always shows what Windows will
//! actually do, including a change made in Task Manager or Task Scheduler.
//! Everything that registers or inspects the startup entry is in this file.

use std::{
    env,
    path::{Path, PathBuf},
};

use windows::{
    core::HSTRING,
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, E_ACCESSDENIED, WIN32_ERROR},
        System::Registry::{
            RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
            RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
        },
    },
};

use crate::elevation::{is_elevated, user_sid};

/// The per-user list of programs Windows starts at sign-in.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Task Manager's "Startup apps" page records an entry it disabled.
/// The `Run` value stays; a value here with an odd first byte turns it off.
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
/// The value name in both keys, and the scheduled task's name; also what
/// Task Manager and Task Scheduler list.
const VALUE_NAME: &str = "Window Switcher";
const TASK_NAME: &str = VALUE_NAME;

/// True when Windows will start this app at sign-in: the `Run` entry exists
/// and Task Manager has not disabled it, or the scheduled task exists and is
/// enabled.
pub(crate) fn is_enabled() -> bool {
    let registered = match read_string(RUN_KEY) {
        Ok(command) => !command.trim().is_empty(),
        Err(error) => {
            if error != ERROR_FILE_NOT_FOUND {
                log::warn!("unable to read the startup entry: {error:?}");
            }
            false
        }
    };
    let run_enabled = registered
        && !read_binary(APPROVED_KEY).is_some_and(|flags| approved_flags_disabled(&flags));
    run_enabled || query_task().is_some_and(|task| task.enabled)
}

/// Turns starting at sign-in on or off for the running executable, with the
/// mechanism `run_as_admin` calls for (see the module comment).
pub(crate) fn set_enabled(enabled: bool, run_as_admin: bool) -> Result<(), String> {
    let exe = env::current_exe()
        .map_err(|error| format!("unable to find this program's path: {error}"))?;
    let wanted = Wanted {
        start: enabled,
        admin: run_as_admin,
        elevated: is_elevated(),
        refresh: true,
    };
    apply(&plan(&registered(), wanted, &exe))
}

/// Moves an existing startup entry to the mechanism `run_as_admin` calls for
/// (e.g. from `Run` to the scheduled task once the app runs elevated),
/// keeping the executable it starts. Does nothing when starting at sign-in
/// is off. Called at launch and when "Run as administrator" changes.
pub(crate) fn reconcile(run_as_admin: bool) -> Result<(), String> {
    if !is_enabled() {
        return Ok(());
    }
    let exe = env::current_exe()
        .map_err(|error| format!("unable to find this program's path: {error}"))?;
    let wanted = Wanted {
        start: true,
        admin: run_as_admin,
        elevated: is_elevated(),
        refresh: false,
    };
    apply(&plan(&registered(), wanted, &exe))
}

/// What is registered now, with the executable each entry starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Registered {
    run: Option<PathBuf>,
    /// `Some` when the task exists; the path inside is `None` when this
    /// process may not read the task (it was registered elevated).
    task: Option<Option<PathBuf>>,
}

fn registered() -> Registered {
    Registered {
        run: read_string(RUN_KEY)
            .ok()
            .filter(|command| !command.trim().is_empty())
            .map(|command| command_path(&command)),
        task: query_task().map(|task| task.path),
    }
}

#[derive(Clone, Copy, Debug)]
struct Wanted {
    /// Start at sign-in.
    start: bool,
    /// "Run as administrator" is on.
    admin: bool,
    /// This process is elevated, so it may register the task.
    elevated: bool,
    /// The user (re)turned the option on: point the entry at the running
    /// executable, rather than only moving an existing one.
    refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    WriteRun(PathBuf),
    CreateTask(PathBuf),
    DeleteRun,
    DeleteTask,
}

/// The changes that take `current` to what `wanted` calls for. A new entry
/// is always created before the old one is deleted, so a failure part way
/// leaves the app starting one way or the other, never not at all.
fn plan(current: &Registered, wanted: Wanted, exe: &Path) -> Vec<Step> {
    let path = if wanted.refresh {
        exe.to_path_buf()
    } else {
        current
            .run
            .clone()
            .or_else(|| current.task.clone().flatten())
            .unwrap_or_else(|| exe.to_path_buf())
    };
    let mut steps = Vec::new();
    if !wanted.start {
        if current.run.is_some() {
            steps.push(Step::DeleteRun);
        }
        if current.task.is_some() {
            steps.push(Step::DeleteTask);
        }
    } else if wanted.admin && (wanted.elevated || current.task.is_some()) {
        // Without elevation an existing task is kept as it is: it still
        // works, and only an elevated process could rewrite it.
        if current.task.is_none() || (wanted.refresh && wanted.elevated) {
            steps.push(Step::CreateTask(path));
        }
        if current.run.is_some() {
            steps.push(Step::DeleteRun);
        }
    } else {
        if wanted.refresh || current.run.is_none() {
            steps.push(Step::WriteRun(path));
        }
        if current.task.is_some() {
            steps.push(Step::DeleteTask);
        }
    }
    steps
}

fn apply(steps: &[Step]) -> Result<(), String> {
    for step in steps {
        log::info!("startup entry: {step:?}");
        match step {
            Step::WriteRun(exe) => write_string(RUN_KEY, &run_command(exe))
                .map_err(|error| format!("unable to add the startup entry: {error:?}"))?,
            Step::DeleteRun => delete_value(RUN_KEY)
                .map_err(|error| format!("unable to remove the startup entry: {error:?}"))?,
            Step::CreateTask(exe) => {
                let sid = user_sid()
                    .map_err(|error| format!("unable to identify the current user: {error}"))?;
                task::register(TASK_NAME, &task_xml(exe, &sid, HIGHEST_AVAILABLE))
                    .map_err(|error| format!("unable to create the startup task: {error}"))?
            }
            Step::DeleteTask => task::delete(TASK_NAME).map_err(|error| {
                if error.code() == E_ACCESSDENIED {
                    "the startup task can only be removed by Window Switcher running as administrator"
                        .to_string()
                } else {
                    format!("unable to remove the startup task: {error}")
                }
            })?,
        }
        if matches!(step, Step::WriteRun(_) | Step::DeleteRun) {
            // Drop a "disabled" mark Task Manager may have left: it would
            // keep a fresh entry from running, and is stale once the entry
            // is gone.
            if let Err(error) = delete_value(APPROVED_KEY) {
                log::warn!("unable to clear the Task Manager startup flag: {error:?}");
            }
        }
    }
    Ok(())
}

/// The command line stored in `Run`: the executable path, always quoted so a
/// path with spaces (e.g. `...\Programs\Window Switcher\...`) runs as one.
fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// The executable a `Run` command line starts.
fn command_path(command: &str) -> PathBuf {
    let command = command.trim();
    match command.strip_prefix('"') {
        Some(rest) => PathBuf::from(rest.split('"').next().unwrap_or(rest)),
        None => PathBuf::from(command.split(' ').next().unwrap_or(command)),
    }
}

/// Task Manager writes 12 bytes; an odd first byte (normally 3) means the
/// user disabled the entry, an even one (normally 2) means enabled.
fn approved_flags_disabled(flags: &[u8]) -> bool {
    flags.first().is_some_and(|first| first & 1 == 1)
}

// ---------------------------------------------------------------------------
// The scheduled task
// ---------------------------------------------------------------------------

/// "Run with highest privileges".
const HIGHEST_AVAILABLE: &str = "HighestAvailable";

struct TaskState {
    enabled: bool,
    /// `None` when the task exists but this process may not read it.
    path: Option<PathBuf>,
}

fn query_task() -> Option<TaskState> {
    match task::query(TASK_NAME) {
        Ok(Some(xml_and_enabled)) => {
            let (xml, enabled) = xml_and_enabled;
            Some(TaskState {
                enabled,
                path: task_command(&xml),
            })
        }
        Ok(None) => None,
        // A task registered elevated may be unreadable without elevation;
        // being refused shows it exists. Assume it is enabled.
        Err(error) if error.code() == E_ACCESSDENIED => Some(TaskState {
            enabled: true,
            path: None,
        }),
        Err(error) => {
            log::warn!("unable to read the startup task: {error}");
            None
        }
    }
}

/// The task definition: at this user's sign-in, start `exe` with the
/// highest rights the user has (elevated for an administrator), with no
/// time limit and on battery too.
fn task_xml(exe: &Path, user_sid: &str, run_level: &str) -> String {
    let command = xml_escape(&exe.display().to_string());
    let directory = exe
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| {
            format!(
                "\n      <WorkingDirectory>{}</WorkingDirectory>",
                xml_escape(&directory.display().to_string())
            )
        })
        .unwrap_or_default();
    let user = xml_escape(user_sid);
    format!(
        r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Window Switcher with administrator rights when you sign in. Managed by the "Start with Windows" and "Run as administrator" settings.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>{directory}
    </Exec>
  </Actions>
</Task>"#
    )
}

/// The executable in a task definition's first `<Command>`.
fn task_command(xml: &str) -> Option<PathBuf> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    let command = xml_unescape(xml[start..end].trim());
    // Task Scheduler accepts a quoted command too.
    Some(PathBuf::from(command.trim_matches('"')))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The Task Scheduler COM API. Each call connects afresh: they are rare
/// (opening Settings, changing a setting, launch).
mod task {
    use windows::{
        core::{Result, BSTR},
        Win32::{
            Foundation::ERROR_FILE_NOT_FOUND,
            System::{
                Com::{
                    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
                    COINIT_APARTMENTTHREADED,
                },
                TaskScheduler::{
                    ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                },
                Variant::VARIANT,
            },
        },
    };

    fn with_root_folder<T>(use_folder: impl FnOnce(&ITaskFolder) -> Result<T>) -> Result<T> {
        // SAFETY: COM is released (when this call initialized it) only after
        // every interface below has been dropped.
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let result = (|| unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let none = VARIANT::default();
            service.Connect(&none, &none, &none, &none)?;
            let folder = service.GetFolder(&BSTR::from("\\"))?;
            use_folder(&folder)
        })();
        if initialized {
            unsafe { CoUninitialize() };
        }
        result
    }

    fn not_found(error: &windows::core::Error) -> bool {
        error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
    }

    /// The task's XML definition and whether it is enabled, or `None` when
    /// there is no such task.
    pub(super) fn query(name: &str) -> Result<Option<(String, bool)>> {
        with_root_folder(|folder| unsafe {
            match folder.GetTask(&BSTR::from(name)) {
                Ok(task) => Ok(Some((task.Xml()?.to_string(), task.Enabled()?.0 != 0))),
                Err(error) if not_found(&error) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    pub(super) fn register(name: &str, xml: &str) -> Result<()> {
        with_root_folder(|folder| unsafe {
            let none = VARIANT::default();
            folder.RegisterTask(
                &BSTR::from(name),
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &none,
                &none,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &none,
            )?;
            Ok(())
        })
    }

    /// Deletes the task; one that does not exist counts as deleted.
    pub(super) fn delete(name: &str) -> Result<()> {
        with_root_folder(|folder| unsafe {
            match folder.DeleteTask(&BSTR::from(name), 0) {
                Err(error) if not_found(&error) => Ok(()),
                result => result,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

fn read_string(subkey: &str) -> Result<String, WIN32_ERROR> {
    let (subkey, name) = (HSTRING::from(subkey), HSTRING::from(VALUE_NAME));
    let mut size = 0u32;
    // SAFETY: the strings outlive the calls; the second call writes at most
    // `size` bytes into a buffer of that many bytes.
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        );
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let length = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }
}

fn read_binary(subkey: &str) -> Option<Vec<u8>> {
    let (subkey, name) = (HSTRING::from(subkey), HSTRING::from(VALUE_NAME));
    let mut buffer = [0u8; 64];
    let mut size = buffer.len() as u32;
    // SAFETY: `size` is the buffer's length in bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &name,
            RRF_RT_REG_BINARY,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then(|| buffer[..size as usize].to_vec())
}

fn write_string(subkey: &str, value: &str) -> Result<(), WIN32_ERROR> {
    let data: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `data` is a NUL-terminated UTF-16 string of `cbdata` bytes.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(VALUE_NAME),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(status)
    }
}

/// Deletes the value; one that does not exist counts as deleted.
fn delete_value(subkey: &str) -> Result<(), WIN32_ERROR> {
    // SAFETY: both strings outlive the call.
    let status = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(VALUE_NAME),
        )
    };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\me\AppData\Local\Programs\Window Switcher\Window Switcher.exe";
    const OLD: &str = r"C:\Old\Window Switcher.exe";

    #[test]
    fn the_command_quotes_the_executable_path() {
        assert_eq!(
            run_command(Path::new(EXE)),
            r#""C:\Users\me\AppData\Local\Programs\Window Switcher\Window Switcher.exe""#
        );
    }

    #[test]
    fn the_executable_is_read_back_from_a_run_command() {
        assert_eq!(command_path(&run_command(Path::new(EXE))), Path::new(EXE));
        assert_eq!(
            command_path(r#""C:\A B\x.exe" --flag"#),
            Path::new(r"C:\A B\x.exe")
        );
        assert_eq!(command_path(r"C:\x.exe --flag"), Path::new(r"C:\x.exe"));
    }

    #[test]
    fn task_manager_flags_decide_whether_the_entry_is_disabled() {
        assert!(!approved_flags_disabled(&[
            2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]));
        assert!(approved_flags_disabled(&[
            3, 0, 0, 0, 0x10, 0x20, 0, 0, 0, 0, 0, 0
        ]));
        assert!(!approved_flags_disabled(&[6]));
        assert!(approved_flags_disabled(&[7]));
        assert!(!approved_flags_disabled(&[]));
    }

    fn wanted(start: bool, admin: bool, elevated: bool, refresh: bool) -> Wanted {
        Wanted {
            start,
            admin,
            elevated,
            refresh,
        }
    }

    fn run_only() -> Registered {
        Registered {
            run: Some(OLD.into()),
            task: None,
        }
    }

    fn task_only() -> Registered {
        Registered {
            run: None,
            task: Some(Some(OLD.into())),
        }
    }

    fn exe() -> &'static Path {
        Path::new(EXE)
    }

    #[test]
    fn turning_startup_on_without_admin_writes_the_run_value() {
        for elevated in [false, true] {
            assert_eq!(
                plan(
                    &Registered::default(),
                    wanted(true, false, elevated, true),
                    exe()
                ),
                [Step::WriteRun(EXE.into())]
            );
        }
        // Turning it on again points the entry at the running copy.
        assert_eq!(
            plan(&run_only(), wanted(true, false, false, true), exe()),
            [Step::WriteRun(EXE.into())]
        );
    }

    #[test]
    fn turning_startup_on_with_admin_creates_the_task_when_elevated() {
        assert_eq!(
            plan(
                &Registered::default(),
                wanted(true, true, true, true),
                exe()
            ),
            [Step::CreateTask(EXE.into())]
        );
        assert_eq!(
            plan(&run_only(), wanted(true, true, true, true), exe()),
            [Step::CreateTask(EXE.into()), Step::DeleteRun]
        );
    }

    #[test]
    fn without_elevation_admin_startup_falls_back_to_the_run_value() {
        assert_eq!(
            plan(
                &Registered::default(),
                wanted(true, true, false, true),
                exe()
            ),
            [Step::WriteRun(EXE.into())]
        );
        // An existing task is kept: only an elevated process could rewrite it.
        assert_eq!(
            plan(&task_only(), wanted(true, true, false, true), exe()),
            []
        );
        let both = Registered {
            run: Some(OLD.into()),
            task: Some(None),
        };
        assert_eq!(
            plan(&both, wanted(true, true, false, true), exe()),
            [Step::DeleteRun]
        );
    }

    #[test]
    fn turning_startup_off_removes_whatever_is_registered() {
        for (admin, elevated) in [(false, false), (true, true), (true, false)] {
            let wanted = wanted(false, admin, elevated, true);
            assert_eq!(plan(&Registered::default(), wanted, exe()), []);
            assert_eq!(plan(&run_only(), wanted, exe()), [Step::DeleteRun]);
            assert_eq!(plan(&task_only(), wanted, exe()), [Step::DeleteTask]);
        }
    }

    #[test]
    fn reconciling_moves_the_entry_and_keeps_its_executable() {
        // Admin turned on, now elevated: Run value -> task.
        assert_eq!(
            plan(&run_only(), wanted(true, true, true, false), exe()),
            [Step::CreateTask(OLD.into()), Step::DeleteRun]
        );
        // Admin turned off: task -> Run value.
        assert_eq!(
            plan(&task_only(), wanted(true, false, true, false), exe()),
            [Step::WriteRun(OLD.into()), Step::DeleteTask]
        );
        // Already right: nothing to do, and the path is left alone.
        assert_eq!(
            plan(&run_only(), wanted(true, false, false, false), exe()),
            []
        );
        assert_eq!(
            plan(&task_only(), wanted(true, true, true, false), exe()),
            []
        );
        // Admin on but not elevated (the UAC prompt was declined): stay on Run.
        assert_eq!(
            plan(&run_only(), wanted(true, true, false, false), exe()),
            []
        );
        // An unreadable task falls back to the running executable.
        let unreadable = Registered {
            run: None,
            task: Some(None),
        };
        assert_eq!(
            plan(&unreadable, wanted(true, false, true, false), exe()),
            [Step::WriteRun(EXE.into()), Step::DeleteTask]
        );
    }

    const SID: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn the_task_starts_the_executable_elevated_at_this_users_sign_in() {
        let xml = task_xml(Path::new(EXE), SID, HIGHEST_AVAILABLE);
        assert!(xml.contains(&format!(
            "<LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{SID}</UserId>"
        )));
        assert!(xml.contains(&format!("<UserId>{SID}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>HighestAvailable</RunLevel>")));
        assert!(xml.contains(&format!("<Command>{EXE}</Command>")));
        assert!(xml.contains(
            r"<WorkingDirectory>C:\Users\me\AppData\Local\Programs\Window Switcher</WorkingDirectory>"
        ));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert_eq!(task_command(&xml).as_deref(), Some(Path::new(EXE)));
    }

    #[test]
    fn task_paths_are_escaped_and_read_back() {
        let exe = Path::new(r"C:\Tom & Jerry's <apps>\x.exe");
        let xml = task_xml(exe, SID, HIGHEST_AVAILABLE);
        assert!(xml.contains(r"<Command>C:\Tom &amp; Jerry&apos;s &lt;apps&gt;\x.exe</Command>"));
        assert_eq!(task_command(&xml).as_deref(), Some(exe));
        assert_eq!(
            task_command(r#"<Exec><Command>"C:\a b\x.exe"</Command></Exec>"#).as_deref(),
            Some(Path::new(r"C:\a b\x.exe"))
        );
        assert_eq!(task_command("<Task/>"), None);
    }

    /// Registers, reads and deletes a real task under a test name, with
    /// normal rights (an elevated-rights task needs an elevated test run).
    /// Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "creates and removes a scheduled task on this machine"]
    fn a_task_round_trips_through_task_scheduler() {
        let name = "Window Switcher test task";
        let sid = user_sid().unwrap();
        let exe = env::current_exe().unwrap();
        let _ = task::delete(name);
        task::register(name, &task_xml(&exe, &sid, "LeastPrivilege")).unwrap();
        let (xml, enabled) = task::query(name).unwrap().expect("the task exists");
        assert!(enabled);
        assert_eq!(task_command(&xml).as_deref(), Some(exe.as_path()));
        task::delete(name).unwrap();
        assert!(task::query(name).unwrap().is_none());
        task::delete(name).unwrap();
    }
}
