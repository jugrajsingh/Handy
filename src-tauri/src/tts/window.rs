use crate::settings::OverlayPosition;
use tauri::Manager;
#[derive(Debug, Clone, PartialEq)]
pub struct WindowPolicy {
    pub width: f64,
    pub height: f64,
    pub focusable: bool,
    pub top: bool,
    pub keyboard_on_demand: bool,
}
pub fn policy(position: OverlayPosition, layer_shell: bool) -> WindowPolicy {
    WindowPolicy {
        width: 560.0,
        height: 200.0,
        focusable: true,
        top: position == OverlayPosition::Top,
        keyboard_on_demand: layer_shell,
    }
}
trait Focusable: Sized {
    fn focusable(self, focusable: bool) -> Self;
}
impl<'a, R: tauri::Runtime, M: tauri::Manager<R>> Focusable
    for tauri::WebviewWindowBuilder<'a, R, M>
{
    fn focusable(self, focusable: bool) -> Self {
        self.focusable(focusable)
    }
}
fn configure_builder<B: Focusable>(builder: B, policy: &WindowPolicy) -> B {
    builder.focusable(policy.focusable)
}
#[cfg(target_os = "linux")]
trait KeyboardSurface {
    fn set_keyboard_mode(&self, mode: gtk_layer_shell::KeyboardMode);
}
#[cfg(target_os = "linux")]
impl KeyboardSurface for gtk::ApplicationWindow {
    fn set_keyboard_mode(&self, mode: gtk_layer_shell::KeyboardMode) {
        gtk_layer_shell::LayerShell::set_keyboard_mode(self, mode);
    }
}
#[cfg(target_os = "linux")]
fn configure_keyboard(surface: &impl KeyboardSurface, policy: &WindowPolicy) {
    surface.set_keyboard_mode(if policy.keyboard_on_demand {
        gtk_layer_shell::KeyboardMode::OnDemand
    } else {
        gtk_layer_shell::KeyboardMode::None
    });
}
pub fn create(app: &tauri::AppHandle, position: OverlayPosition) -> Result<(), String> {
    if app.get_webview_window("read_aloud").is_some() {
        return Ok(());
    }
    let p = policy(position, true);
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        "read_aloud",
        tauri::WebviewUrl::App("src/read-aloud/index.html".into()),
    )
    .title("read_aloud")
    .inner_size(p.width, p.height)
    .visible(false)
    .decorations(false)
    .resizable(false)
    .transparent(true)
    .always_on_top(true)
    .focused(false);
    let window = configure_builder(builder, &p)
        .build()
        .map_err(|_| "tts.errors.window".to_string())?;
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        use gtk_layer_shell::{Layer, LayerShell};
        if !crate::utils::env_flag_enabled("HANDY_NO_GTK_LAYER_SHELL")
            && gtk_layer_shell::is_supported()
        {
            if let Ok(gtk) = window.gtk_window() {
                gtk.init_layer_shell();
                gtk.set_layer(Layer::Overlay);
                configure_keyboard(&gtk, &p);
                gtk.set_exclusive_zone(0);
                crate::overlay::configure_layer_shell_surface(&gtk, position, p.width, p.height);
                gtk.set_size_request(p.width as i32, p.height as i32);
                gtk.resize(1, 1);
            }
        }
    }
    if let Ok(Some(monitor)) = window.primary_monitor() {
        let scale = monitor.scale_factor();
        let area = monitor.work_area();
        let x = (f64::from(area.position.x) + (f64::from(area.size.width) - p.width * scale) / 2.0)
            / scale;
        let y = if p.top {
            f64::from(area.position.y) / scale + 24.0
        } else {
            f64::from(area.position.y + area.size.height as i32) / scale - p.height - 24.0
        };
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(service) = handle.try_state::<std::sync::Arc<super::service::TtsService>>()
            {
                let service = service.inner().clone();
                std::thread::spawn(move || service.close());
            }
            hide(&handle);
        }
    });
    Ok(())
}
pub fn show(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("read_aloud") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}
pub fn hide(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("read_aloud") {
        let _ = w.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn popup_is_focusable_and_layer_shell_keyboard_policy_is_on_demand() {
        struct Builder {
            applied: Option<bool>,
        }
        impl Focusable for Builder {
            fn focusable(mut self, focusable: bool) -> Self {
                self.applied = Some(focusable);
                self
            }
        }
        let top = policy(OverlayPosition::Top, true);
        assert_eq!((top.width, top.height), (560.0, 200.0));
        assert!(top.focusable && top.top && top.keyboard_on_demand);
        assert_eq!(
            configure_builder(Builder { applied: None }, &top).applied,
            Some(true)
        );
        let bottom = policy(OverlayPosition::Bottom, false);
        assert!(!bottom.top && !bottom.keyboard_on_demand);
        assert!(bottom.focusable);
        assert_eq!(
            configure_builder(Builder { applied: None }, &bottom).applied,
            Some(true)
        );
        #[cfg(target_os = "linux")]
        {
            struct Surface(std::cell::Cell<Option<gtk_layer_shell::KeyboardMode>>);
            impl KeyboardSurface for Surface {
                fn set_keyboard_mode(&self, mode: gtk_layer_shell::KeyboardMode) {
                    self.0.set(Some(mode));
                }
            }
            let surface = Surface(std::cell::Cell::new(None));
            configure_keyboard(&surface, &top);
            assert!(matches!(
                surface.0.get(),
                Some(gtk_layer_shell::KeyboardMode::OnDemand)
            ));
            configure_keyboard(&surface, &bottom);
            assert!(matches!(
                surface.0.get(),
                Some(gtk_layer_shell::KeyboardMode::None)
            ));
        }
    }
}
