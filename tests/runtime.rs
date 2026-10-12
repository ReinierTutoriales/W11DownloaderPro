//! Preparación real del entorno de yt-dlp. Descarga desde PyPI: solo se ejecuta con W11DL_RUN_SETUP=1.
use w11dl::runtime;

#[test]
fn setup_crea_entorno_con_yt_dlp_fijado() {
    if std::env::var_os("W11DL_RUN_SETUP").is_none() {
        eprintln!("omitido: define W11DL_RUN_SETUP=1 para descargar yt-dlp desde PyPI");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let st = runtime::setup(dir.path()).expect("setup debe completar");
    assert!(st.ready, "{}", st.message);
    let again = runtime::check(dir.path());
    assert!(again.ready, "{}", again.message);
}
