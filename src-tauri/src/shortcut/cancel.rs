use super::policy::CommandContext;
use crate::settings::{KeyboardImplementation, ShortcutBinding};
use std::sync::Mutex;
use tauri::AppHandle;

#[derive(Clone, Copy)]
pub(crate) enum Owner {
    Dictation,
    Reading,
}

#[derive(Default)]
struct Owners {
    dictation: bool,
    reading: bool,
}
impl Owners {
    fn set(&mut self, owner: Owner, active: bool) {
        match owner {
            Owner::Dictation => self.dictation = active,
            Owner::Reading => self.reading = active,
        }
    }
    fn active(&self) -> bool {
        self.reading || (!cfg!(target_os = "linux") && self.dictation)
    }
}

static OWNERS: Mutex<Owners> = Mutex::new(Owners {
    dictation: false,
    reading: false,
});
static REGISTERED: Mutex<Option<(KeyboardImplementation, ShortcutBinding)>> = Mutex::new(None);

fn reconcile_with(
    app: &impl CommandContext,
    active: bool,
    registered: &mut Option<(KeyboardImplementation, ShortcutBinding)>,
    fallback: impl FnOnce(bool),
) -> Result<(), String> {
    fallback(active);
    let settings = app.settings();
    let desired = if active {
        settings
            .bindings
            .get("cancel")
            .cloned()
            .map(|binding| (settings.keyboard_implementation, binding))
    } else {
        None
    };
    if registered
        .as_ref()
        .map(|(implementation, binding)| (implementation, &binding.current_binding))
        == desired
            .as_ref()
            .map(|(implementation, binding)| (implementation, &binding.current_binding))
    {
        return Ok(());
    }
    if let Some((implementation, binding)) = registered.as_ref() {
        app.unregister_native(*implementation, binding.clone())?;
        *registered = None;
    }
    if let Some((implementation, binding)) = desired {
        super::policy::register_nonempty(binding.clone(), |binding| {
            app.register_native(implementation, binding)
        })?;
        *registered = Some((implementation, binding));
    }
    Ok(())
}

pub(crate) fn set(app: &AppHandle, owner: Owner, active: bool) {
    OWNERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .set(owner, active);
    reconcile(app);
}

pub(crate) fn reconcile(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let mut registered = REGISTERED.lock().unwrap_or_else(|p| p.into_inner());
        let active = OWNERS.lock().unwrap_or_else(|p| p.into_inner()).active();
        if let Err(error) = reconcile_with(&app, active, &mut registered, |active| {
            if active {
                crate::secure_input::register_cancel_fallback(&app);
            } else {
                crate::secure_input::unregister_cancel_fallback(&app);
            }
        }) {
            log::error!("Failed to reconcile cancel shortcut: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::policy::RegistrationContext;
    use super::*;
    use crate::settings::{get_default_settings, AppSettings};
    use std::cell::RefCell;

    struct Native {
        actions: RefCell<Vec<String>>,
    }
    struct NoKeys;
    impl super::super::policy::HandyKeysRegistration for NoKeys {
        fn register(&self, _: &ShortcutBinding) -> Result<(), String> {
            panic!()
        }
    }
    impl RegistrationContext for Native {
        type HandyKeys = NoKeys;
        fn settings(&self) -> AppSettings {
            get_default_settings()
        }
        fn save_settings(&self, _: AppSettings) {
            panic!("cancel ownership does not persist settings")
        }
        fn register_native(
            &self,
            _: KeyboardImplementation,
            binding: ShortcutBinding,
        ) -> Result<(), String> {
            self.actions
                .borrow_mut()
                .push(format!("register:{}", binding.id));
            Ok(())
        }
        fn create_handy_keys(&self) -> Result<Self::HandyKeys, String> {
            panic!()
        }
        fn manage_handy_keys(&self, _: Self::HandyKeys) {
            panic!()
        }
    }
    impl CommandContext for Native {
        fn unregister_native(
            &self,
            _: KeyboardImplementation,
            binding: ShortcutBinding,
        ) -> Result<(), String> {
            self.actions
                .borrow_mut()
                .push(format!("unregister:{}", binding.id));
            Ok(())
        }
        fn reconcile_fallback(&self) {}
    }

    #[test]
    fn wbfix_linux_dictation_does_not_register_cancel() {
        let mut owners = Owners::default();
        owners.set(Owner::Dictation, true);
        assert_eq!(owners.active(), !cfg!(target_os = "linux"));
        owners.set(Owner::Reading, true);
        assert!(owners.active());
        owners.set(Owner::Dictation, false);
        assert!(owners.active());
        owners.set(Owner::Reading, false);
        assert!(!owners.active());
    }
    #[test]
    fn fix1_reading_cancel_registration_survives_dictation_ownership() {
        let native = Native {
            actions: RefCell::new(Vec::new()),
        };
        let mut owners = Owners::default();
        let mut registered = None;
        let mut fallback = Vec::new();
        let mut referee = handy_tts_core::referee::Referee::default();
        use handy_tts_core::referee::{Effect, Event};
        let Effect::Capture { ticket } = referee.apply(Event::ReadShortcut)[0] else {
            panic!()
        };
        for effect in referee.apply(Event::Clipboard {
            ticket,
            text: String::new(),
        }) {
            crate::tts::service::reading_cancel_with(&effect, |active| {
                owners.set(Owner::Reading, active)
            });
        }
        reconcile_with(&native, owners.active(), &mut registered, |active| {
            fallback.push(active)
        })
        .unwrap();
        assert_eq!(
            *native.actions.borrow(),
            ["register:cancel"],
            "empty popup opened from Idle owns cancel"
        );
        for (owner, active) in [
            (Owner::Dictation, true),
            (Owner::Reading, false),
            (Owner::Reading, true),
            (Owner::Dictation, false),
        ] {
            owners.set(owner, active);
            reconcile_with(&native, owners.active(), &mut registered, |active| {
                fallback.push(active)
            })
            .unwrap();
        }
        for effect in referee.apply(Event::Close) {
            crate::tts::service::reading_cancel_with(&effect, |active| {
                owners.set(Owner::Reading, active)
            });
        }
        reconcile_with(&native, owners.active(), &mut registered, |active| {
            fallback.push(active)
        })
        .unwrap();
        assert_eq!(
            *native.actions.borrow(),
            if cfg!(target_os = "linux") {
                vec![
                    "register:cancel",
                    "unregister:cancel",
                    "register:cancel",
                    "unregister:cancel",
                ]
            } else {
                vec!["register:cancel", "unregister:cancel"]
            }
        );
        let dictation_only = !cfg!(target_os = "linux");
        assert_eq!(fallback, [true, true, dictation_only, true, true, false]);
        assert!(registered.is_none());
    }
}
