//! Las versiones del programa, de la extensión y de la guía de prueba del paquete deben coincidir.
use std::path::Path;

fn read(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn json_version(path: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(&read(path)).unwrap();
    value["version"].as_str().unwrap().to_owned()
}

#[test]
fn las_versiones_del_paquete_coinciden() {
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        json_version("extension/manifest.json"),
        version,
        "manifest.json"
    );
    assert_eq!(
        json_version("extension/package.json"),
        version,
        "package.json"
    );
    let first = read("packaging/LEEME-PRUEBA.txt");
    assert!(
        first.starts_with(&format!("W11DownloaderPro {version} ")),
        "LEEME-PRUEBA.txt anuncia otra versión: {}",
        first.lines().next().unwrap_or("")
    );
}
