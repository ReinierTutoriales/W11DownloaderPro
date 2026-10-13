**Versión de pruebas.** Paquete para Windows 11 x64 con todo incluido: no hay que instalar Python, yt-dlp ni ffmpeg.

1. Descomprime en una carpeta permanente.
2. `.\w11dl-host.exe register --firefox-id w11dl@reiniertutoriales`
3. En Firefox: `about:debugging#/runtime/this-firefox` → *Cargar complemento temporal* → `extension\manifest.json`.

Las instrucciones completas, qué probar y los límites conocidos están en `LEEME-PRUEBA.txt` dentro del zip.
Verifica la descarga con el archivo `.sha256` adjunto. Los programas de terceros incluidos (yt-dlp, FFmpeg) y sus licencias figuran en `NOTICES.txt`; sus versiones y hashes están fijados en `packaging/third-party.json`.

Los ejecutables no están firmados: Windows SmartScreen puede avisar.
