use anyhow::{Context, Result, ensure};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Devices {
    pub openvino: String,
    pub available: Vec<String>,
}
pub fn list() -> Result<Devices> {
    if let Some(path) = std::env::var_os("HANDY_OPENVINO_LIBRARY") {
        openvino_sys::library::load_from(path)
            .map_err(anyhow::Error::msg)
            .context("load specified OpenVINO C library")?;
    }
    let core = openvino::Core::new().context("load OpenVINO C API")?;
    let version = openvino::version().build_number;
    ensure!(
        version.starts_with("2026.4"),
        "OpenVINO 2026.4 runtime required"
    );
    let available = core
        .available_devices()
        .context("list OpenVINO devices")?
        .into_iter()
        .map(|device| device.to_string())
        .collect();
    Ok(Devices {
        openvino: version,
        available,
    })
}
