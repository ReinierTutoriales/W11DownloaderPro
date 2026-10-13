//! Registro del host de mensajería nativa para Chrome, Edge y Firefox. Solo se ejecuta cuando el
//! usuario lo pide explícitamente: escribe los manifiestos y las claves del registro de su usuario (HKCU).
//!
//! Chromium identifica la extensión por origen (`allowed_origins`); Firefox por su ID de
//! extensión (`allowed_extensions`). Por eso hay dos manifiestos distintos.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;

pub const HOST_NAME: &str = "com.w11dl.host";

/// Qué extensiones registrar. Al menos una debe estar indicada.
#[derive(Debug, Clone, Default)]
pub struct Targets {
    /// ID de extensión de Chromium: 32 letras de la `a` a la `p`.
    pub chromium_id: Option<String>,
    /// ID de extensión de Firefox (`browser_specific_settings.gecko.id`).
    pub firefox_id: Option<String>,
}

/// Un ID de extensión de Chromium tiene 32 letras de la `a` a la `p`.
pub fn valid_chromium_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// Un ID de Firefox usa letras, dígitos y los símbolos `@ . - _`, sin espacios.
pub fn valid_firefox_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@.-_{}".contains(&b))
}

/// Manifiesto para Chromium y Edge.
pub fn chromium_manifest(host_exe: &Path, extension_id: &str) -> String {
    let manifest = json!({
        "name": HOST_NAME,
        "description": "W11DownloaderPro: recibe descargas capturadas por el navegador",
        "path": host_exe.display().to_string(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")],
    });
    serde_json::to_string_pretty(&manifest).expect("manifiesto serializable")
}

/// Manifiesto para Firefox.
pub fn firefox_manifest(host_exe: &Path, firefox_id: &str) -> String {
    let manifest = json!({
        "name": HOST_NAME,
        "description": "W11DownloaderPro: recibe descargas capturadas por el navegador",
        "path": host_exe.display().to_string(),
        "type": "stdio",
        "allowed_extensions": [firefox_id],
    });
    serde_json::to_string_pretty(&manifest).expect("manifiesto serializable")
}

/// Escribe los manifiestos en `manifest_dir` y los registra. Devuelve las rutas escritas.
pub fn register(
    host_exe: &Path,
    targets: &Targets,
    manifest_dir: &Path,
) -> io::Result<Vec<PathBuf>> {
    if targets.chromium_id.is_none() && targets.firefox_id.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "indica --extension-id o --firefox-id",
        ));
    }
    if let Some(id) = &targets.chromium_id
        && !valid_chromium_id(id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ID de extensión de Chromium no válido",
        ));
    }
    if let Some(id) = &targets.firefox_id
        && !valid_firefox_id(id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ID de extensión de Firefox no válido",
        ));
    }

    std::fs::create_dir_all(manifest_dir)?;
    let mut written = Vec::new();

    if let Some(id) = &targets.chromium_id {
        let path = manifest_dir.join(format!("{HOST_NAME}.json"));
        std::fs::write(&path, chromium_manifest(host_exe, id))?;
        set_registry_value(CHROMIUM_KEYS, &path)?;
        written.push(path);
    }
    if let Some(id) = &targets.firefox_id {
        let path = manifest_dir.join(format!("{HOST_NAME}.firefox.json"));
        std::fs::write(&path, firefox_manifest(host_exe, id))?;
        set_registry_value(FIREFOX_KEYS, &path)?;
        written.push(path);
    }
    Ok(written)
}

/// Quita las claves de registro y los manifiestos.
pub fn unregister(manifest_dir: &Path) -> io::Result<()> {
    remove_registry_keys(CHROMIUM_KEYS)?;
    remove_registry_keys(FIREFOX_KEYS)?;
    for name in [
        format!("{HOST_NAME}.json"),
        format!("{HOST_NAME}.firefox.json"),
    ] {
        match std::fs::remove_file(manifest_dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Chrome y Edge, relativas a HKCU.
const CHROMIUM_KEYS: &[&str] = &[
    r"Software\Google\Chrome\NativeMessagingHosts\com.w11dl.host",
    r"Software\Microsoft\Edge\NativeMessagingHosts\com.w11dl.host",
];
/// Firefox, relativa a HKCU.
const FIREFOX_KEYS: &[&str] = &[r"Software\Mozilla\NativeMessagingHosts\com.w11dl.host"];

#[cfg(windows)]
fn set_registry_value(keys: &[&str], manifest: &Path) -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let value = manifest.display().to_string();
    for key in keys {
        let (subkey, _) = hkcu.create_subkey(key)?;
        subkey.set_value("", &value)?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_registry_keys(keys: &[&str]) -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in keys {
        match hkcu.delete_subkey(key) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_registry_value(_keys: &[&str], _manifest: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "el registro solo existe en Windows",
    ))
}

#[cfg(not(windows))]
fn remove_registry_keys(_keys: &[&str]) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_de_chromium_se_valida() {
        assert!(valid_chromium_id("abcdefghijklmnopabcdefghijklmnop"));
        assert!(!valid_chromium_id("abc"));
        assert!(!valid_chromium_id("ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn id_de_firefox_se_valida() {
        assert!(valid_firefox_id("w11dl@reiniertutoriales"));
        assert!(valid_firefox_id("{9a41deb2-1111-4444-8888-abcdefabcdef}"));
        assert!(!valid_firefox_id(""));
        assert!(!valid_firefox_id("con espacio@x"));
        assert!(!valid_firefox_id("x;rm -rf"));
    }

    #[test]
    fn manifiesto_chromium_limita_el_origen() {
        let text = chromium_manifest(
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
        assert!(value.get("allowed_extensions").is_none());
    }

    #[test]
    fn manifiesto_firefox_usa_allowed_extensions() {
        let text = firefox_manifest(
            Path::new(r"C:\apps\w11dl-host.exe"),
            "w11dl@reiniertutoriales",
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["allowed_extensions"],
            json!(["w11dl@reiniertutoriales"])
        );
        assert!(value.get("allowed_origins").is_none());
    }

    #[test]
    fn register_exige_al_menos_un_destino() {
        let dir = tempfile::tempdir().unwrap();
        let err = register(Path::new("x.exe"), &Targets::default(), dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
