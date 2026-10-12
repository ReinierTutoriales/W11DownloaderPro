# W11DownloaderPro

Motor de descargas acelerado para Windows 11. Fase 1: HTTP/HTTPS con segmentación por rangos, reanudación y verificación SHA-256.

## Uso

```
w11dl <URL> [-o salida] [-c conexiones] [--max-chunk bytes] [--sha256 hex]
```

- Reparte el archivo en fragmentos que las conexiones reclaman dinámicamente.
- Si el servidor no atiende rangos, descarga en un solo flujo (sin reanudación).
- Si se interrumpe, al repetir el comando retoma desde el estado guardado en `<salida>.part.state`.
- El archivo final solo aparece al completarse y, si se indicó `--sha256`, al verificar el hash.

## Desarrollo

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-gnu --all-targets
```

Políticas del proyecto: ver `POLITICAS.md`.
