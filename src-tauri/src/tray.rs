//! System tray icon and menu.
//!
//! The tray is driven by a single *desired state* snapshot ([`TrayDesired`])
//! that callers update through [`set_tray_state`], [`refresh_tray_icon`] and
//! [`update_tray_menu`]. Every such call just records intent and schedules a
//! single applier on the main thread, which diffs the desired snapshot against
//! what is currently displayed and touches the native tray only for the parts
//! that actually changed. Requests that arrive while an apply is pending are
//! coalesced into it, so bursts of state changes never queue up native work.
//!
//! Why: native tray updates are the lever we control for the macOS tray
//! disappearance bug (tauri-apps/tauri#12060, Handy #1948). Before this, every
//! recording cycle rebuilt the full menu 3-6 times from several threads, and
//! concurrent rebuilds could interleave and leave a stale menu behind.
//!
//! Exception: [`set_tray_visibility`] and [`recreate_tray_icon`] call the tray
//! directly. Visibility is a separate attribute that never participates in the
//! icon/menu diff, both are rare and user-initiated, and Tauri marshals them
//! onto the main thread so they serialize with the applier anyway. Re-showing
//! a hidden tray relies on tray-icon recreating it from the last applied
//! icon/menu/tooltip, so those must only ever be set through the applier.

use crate::local_llm::{manager::LocalLlmStateKind, registry, LocalLlmManager, LocalLlmStatus};
use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings;
use crate::tray_i18n::get_tray_translations;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Manager, Theme};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayIconState {
    Idle,
    Recording,
    Transcribing,
}

impl TrayIconState {
    /// Recording and Transcribing share the same menu ("Cancel" instead of the
    /// model submenu), so only the idle/busy distinction matters for the menu.
    fn is_busy(self) -> bool {
        self != TrayIconState::Idle
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CleanupMenu {
    state: LocalLlmStateKind,
    model_name: String,
}

fn cleanup_unload_enabled(cleanup: &CleanupMenu) -> bool {
    cleanup.state == LocalLlmStateKind::Ready
}

fn tray_label<'a>(localized: &'a str, english: &'a str) -> &'a str {
    if localized.is_empty() {
        english
    } else {
        localized
    }
}

fn cleanup_menu(
    busy: bool,
    provider: &str,
    selected_model: Option<&str>,
    no_cleanup_model: &str,
    status: impl FnOnce() -> Option<LocalLlmStatus>,
) -> Option<CleanupMenu> {
    if busy || provider != settings::LOCAL_LLM_PROVIDER_ID {
        return None;
    }
    status().map(|status| {
        let id = status.model_id.as_deref().or(selected_model);
        let model_name = id
            .and_then(registry::find)
            .map(|entry| entry.display_name.to_string())
            .unwrap_or_else(|| no_cleanup_model.to_string());
        CleanupMenu {
            state: status.state,
            model_name,
        }
    })
}

/// Dispatches the blocking cleanup unload on a std thread.
pub(crate) fn unload_cleanup_on_worker(
    unload: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(unload)
}

/// Everything the tray *menu* (and tooltip) depends on. When two snapshots
/// compare equal the menu is not rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuInputs {
    busy: bool,
    cleanup: Option<CleanupMenu>,
    warning: bool,
    model_loaded: bool,
    selected_model: String,
    /// `(id, name)` of downloaded models, sorted by name.
    downloaded_models: Vec<(String, String)>,
    locale: String,
    update_checks_enabled: bool,
}

/// Complete description of what the tray should look like.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrayDesired {
    icon_path: &'static str,
    menu: MenuInputs,
}

struct TrayInner {
    /// Intent set by [`set_tray_state`].
    icon_state: TrayIconState,
    /// Latest computed snapshot, waiting to be (or just) applied.
    desired: Option<TrayDesired>,
    /// Icon the native tray currently shows. Only updated when `set_icon`
    /// succeeds, so a failed update is retried on the next sync.
    applied_icon: Option<&'static str>,
    /// Inputs the native menu was last successfully built from. The tooltip
    /// is derived from the same inputs and set best-effort alongside the menu;
    /// it is not tracked separately.
    applied_menu: Option<MenuInputs>,
    /// An apply is scheduled on the main thread.
    pending: bool,
    /// Decoded icons by resource path so the main thread never touches disk.
    icons: HashMap<&'static str, Image<'static>>,
    /// Handed out to each sync request in trigger order, so a slow request
    /// can't overwrite the snapshot of one that was triggered after it.
    next_seq: u64,
    /// Sequence number of the request that produced `desired`.
    desired_seq: u64,
}

impl TrayInner {
    fn accept_desired(&mut self, seq: u64, desired: TrayDesired) -> bool {
        if seq < self.desired_seq {
            trace!(
                "tray sync: request {seq} superseded by {}",
                self.desired_seq
            );
            return false;
        }
        self.desired = Some(desired);
        self.desired_seq = seq;
        !std::mem::replace(&mut self.pending, true)
    }

    fn menu_needs_rebuild(&self, desired: &MenuInputs) -> bool {
        self.applied_menu.as_ref() != Some(desired)
    }
}

/// Tauri managed state owning the tray's desired/applied snapshots.
pub struct TrayState(Mutex<TrayInner>);

impl TrayState {
    pub fn new() -> Self {
        Self(Mutex::new(TrayInner {
            icon_state: TrayIconState::Idle,
            desired: None,
            applied_icon: None,
            applied_menu: None,
            pending: false,
            icons: HashMap::new(),
            next_seq: 0,
            desired_seq: 0,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, TrayInner> {
        self.0.lock().unwrap_or_else(|poisoned| {
            warn!("Tray state mutex was poisoned, recovering");
            poisoned.into_inner()
        })
    }
}

impl Default for TrayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppTheme {
    Dark,
    Light,
    Colored, // Pink/colored theme for Linux
}

/// Gets the current app theme, with Linux defaulting to Colored theme
pub fn get_current_theme(app: &AppHandle) -> AppTheme {
    if cfg!(target_os = "linux") {
        // On Linux, always use the colored theme
        AppTheme::Colored
    } else {
        // On Windows the tray icon sits on the taskbar, which follows the
        // *system* theme (SystemUsesLightTheme), not the app theme. With the
        // "Custom" personalization mode the two can differ (e.g. dark taskbar
        // + light apps), and the window theme would pick an icon that is
        // invisible against the taskbar.
        #[cfg(target_os = "windows")]
        if let Some(theme) = windows_taskbar_theme() {
            return theme;
        }

        // On other platforms, map system theme to our app theme
        if let Some(main_window) = app.get_webview_window("main") {
            match main_window.theme().unwrap_or(Theme::Dark) {
                Theme::Light => AppTheme::Light,
                Theme::Dark => AppTheme::Dark,
                _ => AppTheme::Dark, // Default fallback
            }
        } else {
            AppTheme::Dark
        }
    }
}

/// Reads the Windows taskbar theme from the registry.
///
/// Returns None if the value is missing (older Windows 10 builds default to a
/// dark taskbar there, but falling back to the window theme is safer than
/// guessing).
#[cfg(target_os = "windows")]
fn windows_taskbar_theme() -> Option<AppTheme> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let personalize = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .ok()?;
    let system_uses_light: u32 = personalize.get_value("SystemUsesLightTheme").ok()?;
    Some(if system_uses_light == 1 {
        AppTheme::Light
    } else {
        AppTheme::Dark
    })
}

/// Gets the appropriate icon path for the given theme and state.
///
/// `warning` overlays a badge on the idle icon while keyboard shortcuts are
/// blocked (macOS Secure Input); recording/transcribing states keep their
/// normal icons so in-flight activity stays recognizable.
pub fn get_icon_path(theme: AppTheme, state: TrayIconState, warning: bool) -> &'static str {
    if warning && state == TrayIconState::Idle {
        return match theme {
            AppTheme::Dark => "resources/tray_idle_warning.png",
            AppTheme::Light => "resources/tray_idle_warning_dark.png",
            // Linux never sets the warning flag (Secure Input is macOS-only),
            // but fall back to the normal icon just in case.
            AppTheme::Colored => "resources/handy.png",
        };
    }
    match (theme, state) {
        // Dark theme uses light icons
        (AppTheme::Dark, TrayIconState::Idle) => "resources/tray_idle.png",
        (AppTheme::Dark, TrayIconState::Recording) => "resources/tray_recording.png",
        (AppTheme::Dark, TrayIconState::Transcribing) => "resources/tray_transcribing.png",
        // Light theme uses dark icons
        (AppTheme::Light, TrayIconState::Idle) => "resources/tray_idle_dark.png",
        (AppTheme::Light, TrayIconState::Recording) => "resources/tray_recording_dark.png",
        (AppTheme::Light, TrayIconState::Transcribing) => "resources/tray_transcribing_dark.png",
        // Colored theme uses pink icons (for Linux)
        (AppTheme::Colored, TrayIconState::Idle) => "resources/handy.png",
        (AppTheme::Colored, TrayIconState::Recording) => "resources/recording.png",
        (AppTheme::Colored, TrayIconState::Transcribing) => "resources/transcribing.png",
    }
}

/// Sets the recording state shown by the tray (icon + Cancel/model menu).
pub fn set_tray_state(app: &AppHandle, state: TrayIconState) {
    sync_tray_with(app, |inner| inner.icon_state = state);
}

/// Re-syncs the tray after something other than the recording state changed
/// (theme, Secure Input warning). The recording state itself is preserved.
pub fn refresh_tray_icon(app: &AppHandle) {
    sync_tray(app);
}

/// Re-syncs the tray after something the menu depends on changed (model
/// list/selection/loaded state, language, settings).
pub fn update_tray_menu(app: &AppHandle) {
    sync_tray(app);
}

/// Records the current desired tray state and schedules one apply on the main
/// thread (or lets an already-pending apply pick it up). Never blocks on the
/// main thread.
///
/// The snapshot (settings, model list, loaded state) is computed on the
/// *calling* thread on purpose: the main-thread applier must not take manager
/// locks that a worker may hold across slow work (see #1716).
pub fn sync_tray(app: &AppHandle) {
    sync_tray_with(app, |_| {});
}

fn sync_tray_with(app: &AppHandle, update: impl FnOnce(&mut TrayInner)) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };

    // Record intent and claim a sequence number in one critical section, so
    // sequence order == the order in which state changes were requested.
    let (seq, icon_state) = {
        let mut inner = state.lock();
        update(&mut inner);
        inner.next_seq += 1;
        (inner.next_seq, inner.icon_state)
    };

    // Tray not built yet (early secure-input monitor callbacks). The intent
    // is kept and picked up by the first sync after the tray exists.
    if app.try_state::<TrayIcon>().is_none() {
        return;
    }

    let desired = compute_desired(app, icon_state);

    // Decode the icon off the main thread, once per path, outside the lock.
    let needs_icon = !state.lock().icons.contains_key(desired.icon_path);
    let loaded_icon = if needs_icon {
        match load_tray_icon(
            app.path()
                .resolve(desired.icon_path, tauri::path::BaseDirectory::Resource),
        ) {
            Ok(image) => Some(image),
            Err(err) => {
                error!("Failed to load tray icon '{}': {err}", desired.icon_path);
                None
            }
        }
    } else {
        None
    };

    let schedule = {
        let mut inner = state.lock();
        if let Some(image) = loaded_icon {
            inner.icons.insert(desired.icon_path, image);
        }
        inner.accept_desired(seq, desired)
    };

    if schedule {
        post_apply(app);
    } else {
        trace!("tray sync: apply already pending");
    }
}

fn compute_desired(app: &AppHandle, icon_state: TrayIconState) -> TrayDesired {
    let settings = settings::get_settings(app);
    let theme = get_current_theme(app);
    let warning = crate::secure_input::tray_warning_active(app);
    let model_loaded = app.state::<Arc<TranscriptionManager>>().is_model_loaded();

    let mut downloaded_models: Vec<(String, String)> = app
        .state::<Arc<ModelManager>>()
        .get_available_models()
        .into_iter()
        .filter(|m| m.is_downloaded)
        .map(|m| (m.id, m.name))
        .collect();
    downloaded_models.sort_by(|a, b| a.1.cmp(&b.1));

    let strings = get_tray_translations(Some(settings.app_language.clone()));
    let english = get_tray_translations(Some("en".to_string()));
    let cleanup = cleanup_menu(
        icon_state.is_busy(),
        &settings.post_process_provider_id,
        settings.local_llm_model_id.as_deref(),
        tray_label(&strings.no_cleanup_model, &english.no_cleanup_model),
        || {
            app.try_state::<Arc<LocalLlmManager>>()
                .map(|manager| manager.status())
        },
    );

    TrayDesired {
        icon_path: get_icon_path(theme, icon_state, warning),
        menu: MenuInputs {
            busy: icon_state.is_busy(),
            cleanup,
            warning,
            model_loaded,
            selected_model: settings.selected_model,
            downloaded_models,
            locale: settings.app_language,
            update_checks_enabled: settings.update_checks_enabled,
        },
    }
}

fn post_apply(app: &AppHandle) {
    let handle = app.clone();
    if let Err(err) = app.run_on_main_thread(move || apply_on_main(&handle)) {
        // Event loop is gone (shutdown). Clear `pending` so a later call, if
        // any, doesn't wait forever for an apply that will never run.
        error!("Failed to dispatch tray update to the main thread: {err}");
        if let Some(state) = app.try_state::<TrayState>() {
            state.lock().pending = false;
        }
    }
}

/// The single writer to the native tray. Runs on the main thread.
fn apply_on_main(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let Some(tray) = app.try_state::<TrayIcon>() else {
        return;
    };

    let started = Instant::now();
    let (desired, icon, icon_changed, menu_changed) = {
        let mut inner = state.lock();
        inner.pending = false;
        let Some(desired) = inner.desired.clone() else {
            return;
        };
        let icon_changed = inner.applied_icon != Some(desired.icon_path);
        let menu_changed = inner.menu_needs_rebuild(&desired.menu);
        if !icon_changed && !menu_changed {
            trace!("tray apply: nothing changed");
            return;
        }
        let icon = inner.icons.get(desired.icon_path).cloned();
        (desired, icon, icon_changed, menu_changed)
    };

    // Each part is recorded as applied only if its native call succeeded, so a
    // transient failure is retried on the next sync instead of being
    // remembered as displayed.
    let mut icon_ok = false;
    if icon_changed {
        match icon {
            Some(image) => match tray.set_icon_with_as_template(Some(image), true) {
                Ok(()) => icon_ok = true,
                Err(err) => error!("Failed to update tray icon '{}': {err}", desired.icon_path),
            },
            None => error!("Tray icon '{}' is not loaded", desired.icon_path),
        }
    }

    let mut menu_ok = false;
    if menu_changed {
        match build_menu(app, &desired.menu) {
            Ok((menu, tooltip)) => match tray.set_menu(Some(menu)) {
                Ok(()) => {
                    menu_ok = true;
                    // Best-effort: logged, not retried. The tooltip is cosmetic
                    // and can only fail on Windows, where a failing
                    // Shell_NotifyIcon call means the icon is failing too.
                    // Gating `menu_ok` on it would re-run the full menu
                    // rebuild on every sync for the cheapest mutation.
                    if let Err(err) = tray.set_tooltip(Some(tooltip)) {
                        error!("Failed to set tray tooltip: {err}");
                    }
                }
                Err(err) => error!("Failed to set tray menu: {err}"),
            },
            Err(err) => error!("Failed to build tray menu: {err}"),
        }
    }

    {
        let mut inner = state.lock();
        if icon_ok {
            inner.applied_icon = Some(desired.icon_path);
        }
        if menu_ok {
            inner.applied_menu = Some(desired.menu.clone());
        }
    }

    debug!(
        "tray apply: icon={} menu={} busy={} took={:?}",
        if icon_changed {
            desired.icon_path
        } else {
            "unchanged"
        },
        if menu_changed { "rebuilt" } else { "unchanged" },
        desired.menu.busy,
        started.elapsed()
    );
}

fn load_tray_icon(resolved_icon_path: tauri::Result<PathBuf>) -> tauri::Result<Image<'static>> {
    let resolved_icon_path = resolved_icon_path?;
    Image::from_path(&resolved_icon_path).map(Image::to_owned)
}

pub fn tray_tooltip() -> String {
    version_label()
}

fn version_label() -> String {
    if cfg!(debug_assertions) {
        format!("Handy v{} (Dev)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("Handy v{}", env!("CARGO_PKG_VERSION"))
    }
}

/// Builds the tray menu and tooltip for the given inputs. Pure with respect
/// to app state: everything it depends on is in `inputs`, plus the
/// process-constant `HANDY_DISABLE_UPDATER` env flag behind
/// `update_checks_forced_disabled()`, which cannot change during a run.
fn build_menu(app: &AppHandle, inputs: &MenuInputs) -> tauri::Result<(Menu<tauri::Wry>, String)> {
    let strings = get_tray_translations(Some(inputs.locale.clone()));

    // Secure Input warning entry (macOS): clicking opens the settings window
    // where the full warning banner explains the situation. Locales that
    // haven't translated the key yet get the English string rather than a
    // blank menu item (build.rs emits "" for missing keys).
    let secure_input_warning = if inputs.warning {
        let label = if strings.secure_input_warning.is_empty() {
            get_tray_translations(Some("en".to_string())).secure_input_warning
        } else {
            strings.secure_input_warning.clone()
        };
        Some(MenuItem::with_id(
            app,
            "secure_input_warning",
            &label,
            true,
            None::<&str>,
        )?)
    } else {
        None
    };

    // Platform-specific accelerators
    #[cfg(target_os = "macos")]
    let (settings_accelerator, quit_accelerator) = (Some("Cmd+,"), Some("Cmd+Q"));
    #[cfg(not(target_os = "macos"))]
    let (settings_accelerator, quit_accelerator) = (Some("Ctrl+,"), Some("Ctrl+Q"));

    // Create common menu items
    let version_label = version_label();
    let version_i = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)?;
    let settings_i = MenuItem::with_id(
        app,
        "settings",
        &strings.settings,
        true,
        settings_accelerator,
    )?;
    let check_updates_i = MenuItem::with_id(
        app,
        "check_updates",
        &strings.check_updates,
        inputs.update_checks_enabled,
        None::<&str>,
    )?;
    let copy_last_transcript_i = MenuItem::with_id(
        app,
        "copy_last_transcript",
        &strings.copy_last_transcript,
        true,
        None::<&str>,
    )?;
    let quit_i = MenuItem::with_id(app, "quit", &strings.quit, true, quit_accelerator)?;
    let separator = || PredefinedMenuItem::separator(app);

    let menu = if inputs.busy {
        let cancel_i = MenuItem::with_id(app, "cancel", &strings.cancel, true, None::<&str>)?;
        Menu::with_items(
            app,
            &[
                &version_i,
                &separator()?,
                &cancel_i,
                &separator()?,
                &copy_last_transcript_i,
                &separator()?,
                &settings_i,
                &check_updates_i,
                &separator()?,
                &quit_i,
            ],
        )?
    } else {
        // Build model submenu — label is the active model name
        let submenu_label = inputs
            .downloaded_models
            .iter()
            .find(|(id, _)| *id == inputs.selected_model)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| strings.model.clone());

        let model_submenu = Submenu::with_id(app, "model_submenu", &submenu_label, true)?;
        for (id, name) in &inputs.downloaded_models {
            let is_active = *id == inputs.selected_model;
            let item_id = format!("model_select:{}", id);
            let item = CheckMenuItem::with_id(app, &item_id, name, true, is_active, None::<&str>)?;
            model_submenu.append(&item)?;
        }

        let unload_model_i = MenuItem::with_id(
            app,
            "unload_model",
            &strings.unload_model,
            inputs.model_loaded,
            None::<&str>,
        )?;

        let cleanup_items = if let Some(cleanup) = &inputs.cleanup {
            let english = get_tray_translations(Some("en".to_string()));
            let state_label = match cleanup.state {
                LocalLlmStateKind::Ready => {
                    tray_label(&strings.cleanup_ready, &english.cleanup_ready)
                }
                LocalLlmStateKind::Starting | LocalLlmStateKind::Stopping => {
                    tray_label(&strings.cleanup_loading, &english.cleanup_loading)
                }
                LocalLlmStateKind::Failed => {
                    tray_label(&strings.cleanup_error, &english.cleanup_error)
                }
                LocalLlmStateKind::Unloaded => {
                    tray_label(&strings.cleanup_not_loaded, &english.cleanup_not_loaded)
                }
            };
            let prefix = tray_label(&strings.cleanup, &english.cleanup);
            let label = format!("{prefix}: {} ({state_label})", cleanup.model_name);
            Some((
                MenuItem::with_id(app, "cleanup_status", &label, false, None::<&str>)?,
                MenuItem::with_id(
                    app,
                    "unload_post_processing_model",
                    tray_label(
                        &strings.unload_post_processing_model,
                        &english.unload_post_processing_model,
                    ),
                    cleanup_unload_enabled(cleanup),
                    None::<&str>,
                )?,
            ))
        } else {
            None
        };

        let menu = Menu::with_items(
            app,
            &[
                &version_i,
                &separator()?,
                &copy_last_transcript_i,
                &separator()?,
                &model_submenu,
                &unload_model_i,
                &separator()?,
                &settings_i,
                &check_updates_i,
                &separator()?,
                &quit_i,
            ],
        )?;
        if let Some((status_item, unload_item)) = cleanup_items {
            menu.insert(&status_item, 6)?;
            menu.insert(&unload_item, 7)?;
        }
        menu
    };

    // When update checks are forced off (e.g. HANDY_DISABLE_UPDATER, set by
    // the Nix package), the item is dropped from the menu rather than shown
    // disabled — it can never do anything in that case, and a disabled item
    // still shifts every entry below it by one position. A manually-disabled
    // toggle in Debug Settings keeps the old greyed-out behavior via the
    // enabled flag.
    if settings::update_checks_forced_disabled() {
        menu.remove(&check_updates_i)?;
    }

    // Both layouts start with [version, separator, ...]; slot the warning in
    // right below the version line so it's the first actionable thing seen.
    let mut tooltip = version_label;
    if let Some(warning_item) = secure_input_warning {
        menu.insert(&warning_item, 2)?;
        menu.insert(&separator()?, 3)?;
        tooltip = format!("{} — {}", tooltip, warning_item.text().unwrap_or_default());
    }

    Ok((menu, tooltip))
}

fn last_transcript_text(entry: &HistoryEntry) -> &str {
    entry
        .post_processed_text
        .as_deref()
        .unwrap_or(&entry.transcription_text)
}

pub fn set_tray_visibility(app: &AppHandle, visible: bool) {
    let tray = app.state::<TrayIcon>();
    if let Err(e) = tray.set_visible(visible) {
        error!("Failed to set tray visibility: {}", e);
    } else {
        info!("Tray visibility set to: {}", visible);
    }
}

/// Recovery for the macOS tray-disappearance bug (#1948, tauri-apps/tauri#12060):
/// the `NSStatusItem` can silently vanish with no error surfaced to the app.
/// Hiding and re-showing the tray recreates it with its current icon, menu and
/// tooltip. Called when the user "relaunches" Handy while it is already running
/// (`RunEvent::Reopen` for Spotlight/Finder/Dock, the single-instance callback
/// for a second process) — the natural "where did my icon go?" moment — so a
/// relaunch brings the icon back without a full quit.
#[cfg(target_os = "macos")]
pub fn recreate_tray_icon(app: &AppHandle) {
    let no_tray = app
        .try_state::<crate::cli::CliArgs>()
        .map(|args| args.no_tray)
        .unwrap_or(false);
    if no_tray || !settings::get_settings(app).show_tray_icon {
        return;
    }
    let Some(tray) = app.try_state::<TrayIcon>() else {
        return;
    };
    info!("Recreating tray icon on relaunch");
    if let Err(e) = tray.set_visible(false).and_then(|_| tray.set_visible(true)) {
        error!("Failed to recreate tray icon: {}", e);
    }
}

pub fn copy_last_transcript(app: &AppHandle) {
    let history_manager = app.state::<Arc<HistoryManager>>();
    let entry = match history_manager.get_latest_completed_entry() {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            warn!("No completed transcription history entries available for tray copy.");
            return;
        }
        Err(err) => {
            error!(
                "Failed to fetch last completed transcription entry: {}",
                err
            );
            return;
        }
    };

    let text = last_transcript_text(&entry);
    if text.trim().is_empty() {
        warn!("Last completed transcription is empty; skipping tray copy.");
        return;
    }

    if let Err(err) = app.clipboard().write_text(text) {
        error!("Failed to copy last transcript to clipboard: {}", err);
        return;
    }

    info!("Copied last transcript to clipboard via tray.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_llm::manager::LocalLlmStateKind;
    use crate::managers::history::HistoryEntry;

    fn build_entry(transcription: &str, post_processed: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            file_name: "handy-1.wav".to_string(),
            timestamp: 0,
            saved: false,
            title: "Recording".to_string(),
            transcription_text: transcription.to_string(),
            post_processed_text: post_processed.map(|text| text.to_string()),
            post_process_prompt: None,
            post_process_requested: false,
        }
    }

    fn inputs(busy: bool) -> MenuInputs {
        MenuInputs {
            busy,
            cleanup: None,
            warning: false,
            model_loaded: true,
            selected_model: "small".to_string(),
            downloaded_models: vec![("small".to_string(), "Small".to_string())],
            locale: "en".to_string(),
            update_checks_enabled: true,
        }
    }

    fn cleanup(state: LocalLlmStateKind) -> CleanupMenu {
        CleanupMenu {
            state,
            model_name: "S1-mini".into(),
        }
    }

    #[test]
    fn cleanup_unload_is_enabled_only_in_ready_and_snapshot_tracks_status() {
        for state in [
            LocalLlmStateKind::Unloaded,
            LocalLlmStateKind::Starting,
            LocalLlmStateKind::Stopping,
            LocalLlmStateKind::Failed,
        ] {
            assert!(!cleanup_unload_enabled(&cleanup(state)));
        }
        assert!(cleanup_unload_enabled(&cleanup(LocalLlmStateKind::Ready)));
        let mut ready = inputs(false);
        ready.cleanup = Some(cleanup(LocalLlmStateKind::Ready));
        let mut unloaded = ready.clone();
        unloaded.cleanup = Some(cleanup(LocalLlmStateKind::Unloaded));
        assert_ne!(ready, unloaded);
        assert_eq!(
            tray_label("", "Unload Post-processing Model"),
            "Unload Post-processing Model"
        );
        assert_eq!(tray_label("Localized", "English"), "Localized");
    }

    #[test]
    fn cleanup_event_burst_coalesces_and_last_snapshot_wins() {
        let state = TrayState::new();
        let mut scheduled = 0;
        for seq in 1..=100 {
            let mut menu = inputs(false);
            menu.cleanup = Some(cleanup(if seq == 100 {
                LocalLlmStateKind::Ready
            } else {
                LocalLlmStateKind::Starting
            }));
            let desired = TrayDesired {
                icon_path: "resources/tray_idle.png",
                menu,
            };
            if state.lock().accept_desired(seq, desired) {
                scheduled += 1;
            }
        }
        assert_eq!(scheduled, 1);
        let mut inner = state.lock();
        let newest = inner.desired.clone().unwrap();
        assert_eq!(newest.menu.cleanup, Some(cleanup(LocalLlmStateKind::Ready)));
        assert!(!inner.accept_desired(
            99,
            TrayDesired {
                icon_path: "resources/tray_idle.png",
                menu: inputs(false),
            }
        ));
        assert_eq!(inner.desired, Some(newest.clone()));
        inner.pending = false;
        assert!(inner.accept_desired(101, newest));
    }

    #[test]
    fn equal_cleanup_snapshots_do_not_rebuild_and_status_changes_do() {
        let state = TrayState::new();
        let mut ready = inputs(false);
        ready.cleanup = Some(cleanup(LocalLlmStateKind::Ready));
        let mut inner = state.lock();
        inner.applied_menu = Some(ready.clone());
        assert!(!inner.menu_needs_rebuild(&ready));
        let mut unloaded = ready.clone();
        unloaded.cleanup = Some(cleanup(LocalLlmStateKind::Unloaded));
        assert!(inner.menu_needs_rebuild(&unloaded));
    }

    #[test]
    fn cleanup_snapshot_is_idle_and_local_only_and_uses_registry_or_fallback() {
        for (busy, provider) in [(true, settings::LOCAL_LLM_PROVIDER_ID), (false, "openai")] {
            assert_eq!(
                cleanup_menu(busy, provider, None, "No cleanup model", || {
                    panic!("irrelevant cleanup status must not be read")
                }),
                None
            );
        }
        let status = LocalLlmStatus {
            state: LocalLlmStateKind::Ready,
            model_id: Some("s1-mini-q4km".into()),
            error: None,
        };
        let snapshot = cleanup_menu(
            false,
            settings::LOCAL_LLM_PROVIDER_ID,
            Some("unknown"),
            "No cleanup model",
            || Some(status.clone()),
        )
        .unwrap();
        assert_eq!(
            snapshot.model_name,
            registry::find("s1-mini-q4km").unwrap().display_name
        );
        let unloaded = LocalLlmStatus {
            state: LocalLlmStateKind::Unloaded,
            model_id: None,
            error: None,
        };
        let snapshot = cleanup_menu(
            false,
            settings::LOCAL_LLM_PROVIDER_ID,
            Some("s1-mini-q4km"),
            "No cleanup model",
            || Some(unloaded.clone()),
        )
        .unwrap();
        assert_eq!(
            snapshot.model_name,
            registry::find("s1-mini-q4km").unwrap().display_name
        );
        let missing = cleanup_menu(
            false,
            settings::LOCAL_LLM_PROVIDER_ID,
            None,
            "No cleanup model",
            || Some(unloaded),
        )
        .unwrap();
        assert_eq!(missing.model_name, "No cleanup model");
        let failed = LocalLlmStatus {
            state: LocalLlmStateKind::Failed,
            model_id: Some("unknown".into()),
            error: Some("first error".into()),
        };
        let snapshot = cleanup_menu(
            false,
            settings::LOCAL_LLM_PROVIDER_ID,
            None,
            "No cleanup model",
            || Some(failed.clone()),
        )
        .unwrap();
        assert_eq!(snapshot.state, LocalLlmStateKind::Failed);
        assert_eq!(snapshot.model_name, "No cleanup model");
        let mut other_error = failed;
        other_error.error = Some("second error".into());
        assert_eq!(
            Some(snapshot),
            cleanup_menu(
                false,
                settings::LOCAL_LLM_PROVIDER_ID,
                None,
                "No cleanup model",
                || Some(other_error)
            )
        );
        assert_eq!(
            cleanup_menu(
                false,
                settings::LOCAL_LLM_PROVIDER_ID,
                None,
                "No cleanup model",
                || None
            ),
            None
        );
    }

    #[test]
    fn cleanup_unload_runs_on_a_different_thread() {
        let caller = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = unload_cleanup_on_worker(move || {
            sender.send(std::thread::current().id()).unwrap();
        });
        worker.join().unwrap();
        assert_ne!(receiver.recv().unwrap(), caller);
    }

    #[test]
    fn uses_post_processed_text_when_available() {
        let entry = build_entry("raw", Some("processed"));
        assert_eq!(last_transcript_text(&entry), "processed");
    }

    #[test]
    fn falls_back_to_raw_transcription() {
        let entry = build_entry("raw", None);
        assert_eq!(last_transcript_text(&entry), "raw");
    }

    #[test]
    fn tray_icon_resolution_failure_is_returned_instead_of_panicking() {
        assert!(load_tray_icon(Err(tauri::Error::UnknownPath)).is_err());
    }

    #[test]
    fn tray_icon_returns_err_when_file_does_not_exist() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let missing = dir.path().join("does_not_exist.png");
        assert!(load_tray_icon(Ok(missing)).is_err());
    }

    #[test]
    fn recording_and_transcribing_share_a_menu() {
        // The icon differs but the menu inputs are identical, so a
        // Recording -> Transcribing transition must not rebuild the menu.
        let recording = TrayDesired {
            icon_path: "resources/tray_recording.png",
            menu: inputs(TrayIconState::Recording.is_busy()),
        };
        let transcribing = TrayDesired {
            icon_path: "resources/tray_transcribing.png",
            menu: inputs(TrayIconState::Transcribing.is_busy()),
        };
        assert_ne!(recording.icon_path, transcribing.icon_path);
        assert_eq!(recording.menu, transcribing.menu);
    }

    #[test]
    fn idle_and_busy_menus_differ() {
        assert_ne!(inputs(false), inputs(true));
    }
}
