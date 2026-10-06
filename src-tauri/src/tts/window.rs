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
pub fn create(app: &tauri::AppHandle, position: OverlayPosition) -> Result<(), String> {
    if app.get_webview_window("read_aloud").is_some() {
        return Ok(());
    }
    let p = policy(position, true);
    let window = tauri::WebviewWindowBuilder::new(
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
    .focused(false)
    .focusable(true)
    .build()
    .map_err(|_| "tts.errors.window".to_string())?;
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{GtkWindowExt, WidgetExt};
        use gtk_layer_shell::{KeyboardMode, Layer, LayerShell};
        if !crate::utils::env_flag_enabled("HANDY_NO_GTK_LAYER_SHELL")
            && gtk_layer_shell::is_supported()
        {
            if let Ok(gtk) = window.gtk_window() {
                gtk.init_layer_shell();
                gtk.set_layer(Layer::Overlay);
                gtk.set_keyboard_mode(KeyboardMode::OnDemand);
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
        let top = policy(OverlayPosition::Top, true);
        assert_eq!((top.width, top.height), (560.0, 200.0));
        assert!(top.focusable && top.top && top.keyboard_on_demand);
        let bottom = policy(OverlayPosition::Bottom, false);
        assert!(!bottom.top && !bottom.keyboard_on_demand);
        assert!(bottom.focusable);
    }
}
