use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};
use thiserror::Error;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpuDriver {
    pub version: String,
    pub library_dir: PathBuf,
}
#[derive(Debug, Error)]
#[error("tts.errors.driverDiscovery")]
pub struct DriverError;
fn version_parts(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()
        .ok()
}
pub fn discover_driver(root: &Path) -> Result<Option<NpuDriver>, DriverError> {
    let directories = match std::fs::read_dir(root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DriverError),
    };
    let mut choices = vec![];
    for directory in directories {
        let directory = directory.map_err(|_| DriverError)?;
        let version = directory.file_name().to_string_lossy().into_owned();
        let Some(parts) = version_parts(&version) else {
            continue;
        };
        let library_dir = directory.path().join("root/usr/lib/x86_64-linux-gnu");
        if library_dir.join("libze_intel_npu.so.1").is_file() {
            choices.push((
                parts,
                NpuDriver {
                    version,
                    library_dir,
                },
            ));
        }
    }
    choices.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(choices.into_iter().next().map(|(_, driver)| driver))
}
pub fn worker_environment(
    driver: Option<&NpuDriver>,
    parent_library_path: Option<&OsStr>,
) -> BTreeMap<OsString, OsString> {
    let Some(driver) = driver else {
        return BTreeMap::new();
    };
    let mut library_path = driver.library_dir.as_os_str().to_owned();
    if let Some(parent) = parent_library_path.filter(|p| !p.is_empty()) {
        library_path.push(":");
        library_path.push(parent);
    }
    BTreeMap::from([
        (
            OsString::from("ZE_ENABLE_ALT_DRIVERS"),
            driver
                .library_dir
                .join("libze_intel_npu.so.1")
                .into_os_string(),
        ),
        (OsString::from("LD_LIBRARY_PATH"), library_path),
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn driver_environment_changes_only_worker_command_and_keeps_parent_libraries() {
        let before = std::env::var_os("LD_LIBRARY_PATH");
        let driver = NpuDriver {
            version: "1.38.0".into(),
            library_dir: PathBuf::from("/fixture/intel-npu/1.38.0/root/usr/lib/x86_64-linux-gnu"),
        };
        let values = worker_environment(Some(&driver), Some(OsStr::new("/usr/lib/Handy/tts")));
        let mut command = std::process::Command::new("handy-tts");
        command.envs(&values);
        assert_eq!(command.get_envs().count(), 2);
        assert_eq!(
            values[OsStr::new("LD_LIBRARY_PATH")],
            OsStr::new(
                "/fixture/intel-npu/1.38.0/root/usr/lib/x86_64-linux-gnu:/usr/lib/Handy/tts"
            )
        );
        assert_eq!(std::env::var_os("LD_LIBRARY_PATH"), before);
        assert!(worker_environment(None, Some(OsStr::new("/system"))).is_empty());
        assert_eq!(version_parts("1.38.0"), Some(vec![1, 38, 0]));
        assert!(version_parts("env.sh").is_none());
    }
}
