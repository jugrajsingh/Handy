use crate::settings::{AppSettings, KeyboardImplementation, ShortcutBinding};

/// Supplies settings and native registration operations to the shortcut entry points.
pub trait RegistrationContext {
    type HandyKeys: HandyKeysRegistration;

    fn settings(&self) -> AppSettings;
    fn load_settings(&self) -> AppSettings {
        self.settings()
    }
    fn save_settings(&self, settings: AppSettings);
    fn register_native(
        &self,
        implementation: KeyboardImplementation,
        binding: ShortcutBinding,
    ) -> Result<(), String>;
    fn create_handy_keys(&self) -> Result<Self::HandyKeys, String>;
    fn manage_handy_keys(&self, state: Self::HandyKeys);
}

/// Registers a shortcut on the unmanaged HandyKeys state during startup.
pub trait HandyKeysRegistration {
    fn register(&self, binding: &ShortcutBinding) -> Result<(), String>;
}

impl HandyKeysRegistration for super::handy_keys::HandyKeysState {
    fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        self.register(binding)
    }
}

impl RegistrationContext for tauri::AppHandle {
    type HandyKeys = super::handy_keys::HandyKeysState;

    fn settings(&self) -> AppSettings {
        crate::settings::get_settings(self)
    }

    fn load_settings(&self) -> AppSettings {
        crate::settings::load_or_create_app_settings(self)
    }

    fn save_settings(&self, settings: AppSettings) {
        crate::settings::write_settings(self, settings);
    }

    fn register_native(
        &self,
        implementation: KeyboardImplementation,
        binding: ShortcutBinding,
    ) -> Result<(), String> {
        match implementation {
            KeyboardImplementation::Tauri => {
                super::tauri_impl::register_nonempty_shortcut(self, binding)
            }
            KeyboardImplementation::HandyKeys => {
                super::handy_keys::register_nonempty_shortcut(self, binding)
            }
        }
    }

    fn create_handy_keys(&self) -> Result<Self::HandyKeys, String> {
        super::handy_keys::HandyKeysState::new(self.clone())
    }

    fn manage_handy_keys(&self, state: Self::HandyKeys) {
        use tauri::Manager;
        self.manage(state);
    }
}

pub(crate) fn normalized_binding(
    raw: &str,
    implementation: KeyboardImplementation,
) -> Result<String, String> {
    let raw = raw
        .split('+')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("+")
        .to_lowercase();
    match implementation {
        KeyboardImplementation::Tauri => raw
            .parse::<tauri_plugin_global_shortcut::Shortcut>()
            .map(|shortcut| shortcut.into_string().to_lowercase())
            .map_err(|error| error.to_string()),
        KeyboardImplementation::HandyKeys => raw
            .parse::<::handy_keys::Hotkey>()
            .map(|shortcut| shortcut.to_handy_string())
            .map_err(|error| error.to_string()),
    }
}

pub(crate) fn reject_duplicate(settings: &AppSettings, id: &str, raw: &str) -> Result<(), String> {
    let candidate = normalized_binding(raw, settings.keyboard_implementation)?;
    let mut others = settings.bindings.iter().collect::<Vec<_>>();
    others.sort_by(|a, b| a.0.cmp(b.0));
    for (other_id, other) in others {
        if other_id == id || other.current_binding.trim().is_empty() {
            continue;
        }
        if normalized_binding(&other.current_binding, settings.keyboard_implementation)
            .ok()
            .as_ref()
            == Some(&candidate)
        {
            return Err(format!(
                "Shortcut is already assigned to '{}' ({other_id})",
                other.name
            ));
        }
    }
    Ok(())
}

pub(crate) fn clear_binding_with(
    settings: &mut AppSettings,
    id: &str,
    unregister: impl FnOnce(&ShortcutBinding) -> Result<(), String>,
) -> Result<ShortcutBinding, String> {
    if !settings.post_process_enabled
        || !matches!(id, "transcribe" | "transcribe_with_post_process")
    {
        return Err(
            "Only dictation shortcuts can be cleared while post-processing is enabled".into(),
        );
    }
    let other = if id == "transcribe" {
        "transcribe_with_post_process"
    } else {
        "transcribe"
    };
    if settings
        .bindings
        .get(other)
        .is_none_or(|binding| binding.current_binding.trim().is_empty())
    {
        return Err("At least one dictation shortcut must remain assigned".into());
    }
    let mut binding = settings
        .bindings
        .get(id)
        .cloned()
        .ok_or_else(|| format!("Unknown shortcut '{id}'"))?;
    if !binding.current_binding.trim().is_empty() {
        unregister(&binding)?;
    }
    binding.current_binding.clear();
    settings.bindings.insert(id.to_string(), binding.clone());
    Ok(binding)
}

pub(crate) fn restore_raw_binding(settings: &mut AppSettings) -> bool {
    if settings.post_process_enabled {
        return false;
    }
    if let Some(binding) = settings.bindings.get_mut("transcribe") {
        if binding.current_binding.trim().is_empty() {
            binding.current_binding = binding.default_binding.clone();
            return true;
        }
    }
    false
}

pub(crate) fn register_nonempty(
    binding: ShortcutBinding,
    register: impl FnOnce(ShortcutBinding) -> Result<(), String>,
) -> Result<(), String> {
    if binding.current_binding.trim().is_empty() {
        return Ok(());
    }
    register(binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::get_default_settings;

    #[test]
    fn duplicate_is_refused_even_with_no_native_registrations() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let mut settings = get_default_settings();
            settings.keyboard_implementation = implementation;
            settings
                .bindings
                .get_mut("transcribe")
                .unwrap()
                .current_binding = "ctrl+alt+d".into();
            assert!(reject_duplicate(
                &settings,
                "transcribe_with_post_process",
                " ALT + CTRL + D "
            )
            .unwrap_err()
            .contains("transcribe"));
            assert!(reject_duplicate(&settings, "cancel", "alt+ctrl+d").is_err());
            assert!(reject_duplicate(&settings, "transcribe", "ctrl+alt+d").is_ok());
        }
    }
    #[test]
    fn clear_refuses_last_dictation_cancel_and_disabled_feature() {
        let mut settings = get_default_settings();
        settings.post_process_enabled = true;
        assert!(clear_binding_with(&mut settings, "cancel", |_| {
            panic!("cancel must not be unregistered by a clear")
        })
        .is_err());
        clear_binding_with(&mut settings, "transcribe", |_| Ok(())).unwrap();
        assert!(
            clear_binding_with(&mut settings, "transcribe_with_post_process", |_| Ok(())).is_err()
        );
        settings.post_process_enabled = false;
        assert!(clear_binding_with(&mut settings, "transcribe", |_| Ok(())).is_err());
    }
    #[test]
    fn clear_does_not_persist_after_unregister_failure() {
        let mut settings = get_default_settings();
        settings.post_process_enabled = true;
        let before = settings.bindings["transcribe"].current_binding.clone();
        assert!(clear_binding_with(&mut settings, "transcribe", |_| Err(
            "native unregister failed".into()
        ))
        .is_err());
        assert_eq!(settings.bindings["transcribe"].current_binding, before);
    }
    #[test]
    fn raw_is_restored_on_settings_load_after_disabling_cleanup() {
        let mut settings = get_default_settings();
        settings.post_process_enabled = true;
        clear_binding_with(&mut settings, "transcribe", |_| Ok(())).unwrap();
        assert!(!restore_raw_binding(&mut settings));
        settings.post_process_enabled = false;
        assert!(restore_raw_binding(&mut settings));
        assert_eq!(
            settings.bindings["transcribe"].current_binding,
            settings.bindings["transcribe"].default_binding
        );
        assert!(!restore_raw_binding(&mut settings));
    }
    #[test]
    fn registration_skips_empty_on_both_backends() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let mut binding = get_default_settings().bindings["transcribe"].clone();
            binding.current_binding.clear();
            register_nonempty(binding.clone(), |_| {
                panic!("empty shortcut reached {implementation:?}")
            })
            .unwrap();
            binding.current_binding = "ctrl+d".into();
            let mut calls = 0;
            register_nonempty(binding, |_| {
                calls += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(calls, 1);
        }
    }
}

#[cfg(test)]
mod registration_path_tests {
    use super::*;
    use crate::settings::get_default_settings;
    use crate::shortcut::{handy_keys, tauri_impl};
    use std::cell::RefCell;
    use std::rc::Rc;

    struct TestContext {
        settings: RefCell<AppSettings>,
        calls: Rc<RefCell<Vec<(KeyboardImplementation, String)>>>,
        managed: RefCell<bool>,
    }

    struct TestHandyKeys(Rc<RefCell<Vec<(KeyboardImplementation, String)>>>);

    impl HandyKeysRegistration for TestHandyKeys {
        fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
            native_register(KeyboardImplementation::HandyKeys, binding, &self.0)
        }
    }

    fn native_register(
        implementation: KeyboardImplementation,
        binding: &ShortcutBinding,
        calls: &RefCell<Vec<(KeyboardImplementation, String)>>,
    ) -> Result<(), String> {
        assert!(
            !binding.current_binding.trim().is_empty(),
            "blank reached native registration"
        );
        match implementation {
            KeyboardImplementation::Tauri => {
                tauri_impl::validate_shortcut(&binding.current_binding)?
            }
            KeyboardImplementation::HandyKeys => {
                handy_keys::validate_shortcut(&binding.current_binding)?
            }
        }
        calls
            .borrow_mut()
            .push((implementation, binding.id.clone()));
        Ok(())
    }

    impl RegistrationContext for TestContext {
        type HandyKeys = TestHandyKeys;

        fn settings(&self) -> AppSettings {
            self.settings.borrow().clone()
        }

        fn save_settings(&self, settings: AppSettings) {
            *self.settings.borrow_mut() = settings;
        }

        fn register_native(
            &self,
            implementation: KeyboardImplementation,
            binding: ShortcutBinding,
        ) -> Result<(), String> {
            native_register(implementation, &binding, &self.calls)
        }

        fn create_handy_keys(&self) -> Result<Self::HandyKeys, String> {
            Ok(TestHandyKeys(Rc::clone(&self.calls)))
        }

        fn manage_handy_keys(&self, _state: Self::HandyKeys) {
            *self.managed.borrow_mut() = true;
        }
    }

    impl TestContext {
        fn new(implementation: KeyboardImplementation) -> Self {
            let mut settings = get_default_settings();
            settings.keyboard_implementation = implementation;
            settings.post_process_enabled = true;
            settings
                .bindings
                .get_mut("transcribe")
                .unwrap()
                .current_binding = "   ".into();
            Self {
                settings: RefCell::new(settings),
                calls: Rc::default(),
                managed: RefCell::new(false),
            }
        }

        fn assert_clean_only(&self, implementation: KeyboardImplementation) {
            assert_eq!(
                *self.calls.borrow(),
                [(implementation, "transcribe_with_post_process".into())]
            );
        }
    }

    #[test]
    fn backend_registration_entries_skip_blank_bindings() {
        let context = TestContext::new(KeyboardImplementation::Tauri);
        let raw = context.settings().bindings["transcribe"].clone();
        tauri_impl::register_shortcut(&context, raw.clone()).unwrap();
        handy_keys::register_shortcut(&context, raw.clone()).unwrap();
        super::super::register_shortcut(&context, raw).unwrap();
        assert!(context.calls.borrow().is_empty());
        let clean = context.settings().bindings["transcribe_with_post_process"].clone();
        tauri_impl::register_shortcut(&context, clean.clone()).unwrap();
        handy_keys::register_shortcut(&context, clean).unwrap();
        assert_eq!(context.calls.borrow().len(), 2);
    }

    #[test]
    fn startup_and_resume_preserve_optional_raw_on_both_backends() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let context = TestContext::new(implementation);
            match implementation {
                KeyboardImplementation::Tauri => tauri_impl::init_shortcuts(&context),
                KeyboardImplementation::HandyKeys => {
                    handy_keys::init_shortcuts(&context).unwrap();
                    assert!(*context.managed.borrow());
                }
            }
            context.assert_clean_only(implementation);
            context.calls.borrow_mut().clear();
            super::super::resume_all_shortcuts(&context);
            context.assert_clean_only(implementation);
            context.calls.borrow_mut().clear();
            context.settings.borrow_mut().post_process_enabled = false;
            super::super::resume_all_shortcuts(&context);
            assert!(context.calls.borrow().is_empty());
        }
    }

    #[test]
    fn implementation_switch_preserves_blank_instead_of_resetting_it() {
        for implementation in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            let context = TestContext::new(implementation);
            let reset =
                super::super::register_all_shortcuts_for_implementation(&context, implementation);
            assert!(reset.is_empty());
            assert_eq!(
                context.settings().bindings["transcribe"].current_binding,
                "   "
            );
            context.assert_clean_only(implementation);
            context.calls.borrow_mut().clear();
            context
                .settings
                .borrow_mut()
                .bindings
                .get_mut("transcribe")
                .unwrap()
                .current_binding = "invalid-key-name".into();
            let reset =
                super::super::register_all_shortcuts_for_implementation(&context, implementation);
            assert_eq!(reset, ["transcribe"]);
            assert_eq!(
                context.settings().bindings["transcribe"].current_binding,
                context.settings().bindings["transcribe"].default_binding
            );
            assert_eq!(context.calls.borrow().len(), 2);
        }
    }
}
