#!/usr/bin/env python3
"""Empaqueta W11DownloaderPro para Windows x64: ejecutables, extensión, yt-dlp y ffmpeg incluidos.

Descarga las herramientas fijadas en packaging/third-party.json y comprueba su SHA-256 antes de
usarlas; si algún hash no coincide, aborta sin empaquetar. Solo usa la biblioteca estándar, así
que funciona igual en Windows y en Linux.

Uso: python packaging/bundle.py --bin-dir <carpeta con w11dl.exe y w11dl-host.exe>
"""
import argparse
import hashlib
import json
import shutil
import sys
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(url: str, expected: str, cache: Path) -> Path:
    """Descarga `url` (o reutiliza la caché) y exige que su SHA-256 sea `expected`."""
    cache.mkdir(parents=True, exist_ok=True)
    target = cache / expected
    if not target.exists() or sha256(target) != expected:
        print(f"descargando {url}", flush=True)
        tmp = target.with_suffix(".tmp")
        with urllib.request.urlopen(url, timeout=600) as response, tmp.open("wb") as out:
            shutil.copyfileobj(response, out)
        tmp.replace(target)
    actual = sha256(target)
    if actual != expected:
        target.unlink()
        sys.exit(f"ERROR: el SHA-256 de {url} no coincide.\n  esperado {expected}\n  obtenido {actual}")
    print(f"  SHA-256 verificado: {expected[:16]}…", flush=True)
    return target


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", required=True, type=Path, help="carpeta con w11dl.exe y w11dl-host.exe")
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    parser.add_argument("--cache", type=Path, default=ROOT / "dist" / ".cache")
    args = parser.parse_args()

    version = next(
        line.split('"')[1] for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines()
        if line.startswith("version = ")
    )
    pins = json.loads((ROOT / "packaging" / "third-party.json").read_text(encoding="utf-8"))
    name = f"w11dl-{version}-windows-x64"
    folder = args.out / name
    if folder.exists():
        shutil.rmtree(folder)
    (folder / "tools").mkdir(parents=True)
    (folder / "extension").mkdir()

    for exe in ("w11dl.exe", "w11dl-host.exe"):
        source = args.bin_dir / exe
        if not source.is_file():
            sys.exit(f"ERROR: falta {source}")
        shutil.copy2(source, folder / exe)

    ytdlp = fetch(pins["yt-dlp"]["url"], pins["yt-dlp"]["sha256"], args.cache)
    shutil.copy2(ytdlp, folder / "tools" / "yt-dlp.exe")

    ffmpeg_zip = fetch(pins["ffmpeg"]["url"], pins["ffmpeg"]["sha256"], args.cache)
    wanted = {"ffmpeg.exe": "ffmpeg.exe", "ffprobe.exe": "ffprobe.exe", "LICENSE.txt": "FFMPEG-LICENSE.txt"}
    with zipfile.ZipFile(ffmpeg_zip) as z:
        for member in z.namelist():
            leaf = member.rsplit("/", 1)[-1]
            if leaf in wanted and (member.endswith("/bin/" + leaf) or leaf == "LICENSE.txt"):
                with z.open(member) as src, (folder / "tools" / wanted[leaf]).open("wb") as dst:
                    shutil.copyfileobj(src, dst)
    for tool in ("ffmpeg.exe", "ffprobe.exe", "FFMPEG-LICENSE.txt"):
        if not (folder / "tools" / tool).is_file():
            sys.exit(f"ERROR: {tool} no está en el zip de ffmpeg")

    for doc in ("LEEME-PRUEBA.txt", "NOTICES.txt", "third-party.json", "requirements.lock"):
        shutil.copy2(ROOT / "packaging" / doc, folder / doc)
    for doc in ("POLITICAS.md", "README.md"):
        shutil.copy2(ROOT / doc, folder / doc)
    # Solo lo que carga el navegador: sin pruebas ni dependencias de desarrollo.
    ext = ROOT / "extension"
    for item in ("manifest.json", "background.js", "README.md"):
        shutil.copy2(ext / item, folder / "extension" / item)
    for sub in ("lib", "content", "popup"):
        shutil.copytree(ext / sub, folder / "extension" / sub)

    sums = folder / "SHA256SUMS.txt"
    lines = [
        f"{sha256(p)}  {p.relative_to(folder).as_posix()}"
        for p in sorted(folder.rglob("*")) if p.is_file() and p != sums
    ]
    sums.write_text("\n".join(lines) + "\n", encoding="utf-8")

    archive = args.out / f"{name}.zip"
    if archive.exists():
        archive.unlink()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for p in sorted(folder.rglob("*")):
            z.write(p, Path(name) / p.relative_to(folder))
    digest = sha256(archive)
    (args.out / f"{name}.zip.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(f"\n{archive}  ({archive.stat().st_size / 1e6:.0f} MB)\nSHA-256 {digest}")


if __name__ == "__main__":
    main()
