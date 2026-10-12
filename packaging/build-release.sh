#!/usr/bin/env bash
# Empaqueta una versión de pruebas para Windows x64 a partir del árbol actual.
# Uso: packaging/build-release.sh   (requiere el objetivo x86_64-pc-windows-gnu y mingw-w64)
set -euo pipefail
cd "$(dirname "$0")/.."

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
name="w11dl-${version}-windows-x64"
out="dist/${name}"

cargo build --release --target x86_64-pc-windows-gnu
rm -rf "dist/${name}" "dist/${name}.zip"
mkdir -p "${out}/extension"

cp target/x86_64-pc-windows-gnu/release/w11dl.exe target/x86_64-pc-windows-gnu/release/w11dl-host.exe "${out}/"
cp packaging/requirements.lock packaging/LEEME-PRUEBA.txt "${out}/"
cp POLITICAS.md README.md "${out}/" 2>/dev/null || true
# Solo lo que carga el navegador: sin pruebas ni dependencias de desarrollo.
cp -r extension/manifest.json extension/background.js extension/lib extension/content extension/popup extension/README.md "${out}/extension/"

(cd "${out}" && find . -type f ! -name SHA256SUMS.txt -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS.txt)
(cd dist && zip -qr "${name}.zip" "${name}")
sha256sum "dist/${name}.zip" | tee "dist/${name}.zip.sha256"
