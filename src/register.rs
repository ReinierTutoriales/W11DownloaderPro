//! Registro del host de mensajería nativa para Chrome y Edge. Solo se ejecuta cuando el usuario
//! lo pide explícitamente: escribe el manifiesto y las claves del registro de su usuario (HKCU).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

pub const HOST_NAME: &str = "com.w11dl.host";

/// Rutas de registro de cada navegador basado en Chromium, relativas a HKCU.
#[cfg(windows)]
const REGISTRY_KEYS: &[&str] = &[
    r"Software\Google\Chrome\NativeMessagingHosts\com.w11dl.host",
    r"Software\Microsoft\Edge\NativeMessagingHosts\com.w11dl.host",
];

/// Un ID de extensión de Chromium tiene 32 letras de la `a` a la `p`.
pub fn valid_extension_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// Manifiesto que el navegador lee para saber qué ejecutar y qué extensión puede hablarle.
pub fn manifest(host_exe: &Path, extension_id: &str) -> String {
    let manifest = json!({
        "name": HOST_NAME,
        "description": "W11DownloaderPro: recibe descargas capturadas por el navegador",
        "path": host_exe.display().to_string(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")],
    });
    serde_json::to_string_pretty(&manifest).expect("manifiesto serializable")
}

/// Escribe el manifiesto en `manifest_dir` y lo registra en Chrome y Edge.
/// Devuelve la ruta del manifiesto.
pub fn register(host_exe: &Path, extension_id: &str, manifest_dir: &Path) -> io::Result<PathBuf> {
    if !valid_extension_id(extension_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ID de extensión no válido",
        ));
    }
    std::fs::create_dir_all(manifest_dir)?;
    let path = manifest_dir.join(format!("{HOST_NAME}.json"));
    std::fs::write(&path, manifest(host_exe, extension_id))?;
    set_registry_values(&path)?;
    Ok(path)
}

/// Quita las claves de registro. El manifiesto se borra si existe.
pub fn unregister(manifest_dir: &Path) -> io::Result<()> {
    remove_registry_values()?;
    match std::fs::remove_file(manifest_dir.join(format!("{HOST_NAME}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn set_registry_values(manifest: &Path) -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let value = manifest.display().to_string();
    for key in REGISTRY_KEYS {
        let (subkey, _) = hkcu.create_subkey(key)?;
        subkey.set_value("", &value)?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_registry_values() -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in REGISTRY_KEYS {
        match hkcu.delete_subkey(key) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_registry_values(_manifest: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "el registro solo existe en Windows",
    ))
}

#[cfg(not(windows))]
fn remove_registry_values() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_de_extension_se_valida() {
        assert!(valid_extension_id("abcdefghijklmnopabcdefghijklmnop"));
        assert!(!valid_extension_id("abc"));
        assert!(!valid_extension_id("ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP"));
        assert!(!valid_extension_id("qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"));
    }

    #[test]
    fn manifiesto_limita_el_origen_a_la_extension() {
        let text = manifest(
            Path::new(r"C:\apps\w11dl-host.exe"),
            "abcdefghijklmnopabcdefghijklmnop",
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["name"], HOST_NAME);
        assert_eq!(value["type"], "stdio");
        assert_eq!(
            value["allowed_origins"],
            json!(["chrome-extension://abcdefghijklmnopabcdefghijklmnop/"])
        );
    }
}
