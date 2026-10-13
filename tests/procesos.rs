//! Con procesos reales: una carpeta de descargas tiene un único propietario de la cola. El host del
//! navegador y la cola programada (`run-queue`) no pueden escribirse el archivo de la cola a la vez.
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_w11dl-host");

fn spawn_host(dir: &Path) -> Child {
    Command::new(HOST)
        .arg("--out-dir")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn send(child: &mut Child, json: &str) {
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    stdin.write_all(json.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

fn read_frame(child: &mut Child) -> serde_json::Value {
    let out = child.stdout.as_mut().unwrap();
    let mut len = [0u8; 4];
    out.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    out.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Espera a que el proceso sea dueño de la cola: el archivo de bloqueo existe y está tomado.
fn wait_until_owner(dir: &Path) {
    let lock = dir.join(".w11dl-queue.lock");
    let began = Instant::now();
    loop {
        if let Ok(file) = std::fs::File::open(&lock)
            && matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
        {
            return;
        }
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "el host no tomó la cola"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_queue(dir: &Path) -> std::process::Output {
    Command::new(HOST)
        .args(["run-queue", "--after", "nothing", "--out-dir"])
        .arg(dir)
        .output()
        .unwrap()
}

#[test]
fn la_cola_programada_no_pisa_la_cola_de_un_host_abierto() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = spawn_host(dir.path());
    wait_until_owner(dir.path());

    let busy = run_queue(dir.path());
    let stderr = String::from_utf8_lossy(&busy.stderr);
    assert_eq!(busy.status.code(), Some(2), "debía negarse: {stderr}");
    assert!(stderr.contains("otra instancia"), "{stderr}");

    // Al cerrarse el host, la cola queda libre.
    drop(host.stdin.take());
    assert!(host.wait().unwrap().success());
    let free = run_queue(dir.path());
    assert!(
        free.status.success(),
        "{}",
        String::from_utf8_lossy(&free.stderr)
    );
}

#[test]
fn un_segundo_host_funciona_pero_no_guarda_la_cola_y_lo_avisa_una_vez() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = spawn_host(dir.path());
    wait_until_owner(dir.path());
    let before = std::fs::read(dir.path().join(".w11dl-queue.json")).unwrap();

    let mut second = spawn_host(dir.path());
    send(&mut second, r#"{"type":"ping"}"#);
    // Puede llegar antes el aviso de que no guardará la cola; después, el pong.
    let mut warnings = 0;
    loop {
        let msg = read_frame(&mut second);
        match msg["type"].as_str() {
            Some("error") => {
                warnings += 1;
                assert!(
                    msg["message"].as_str().unwrap().contains("otra instancia"),
                    "{msg}"
                );
            }
            Some("pong") => break,
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }
    send(
        &mut second,
        r#"{"type":"enqueue","id":"x","url":"http://127.0.0.1:9/a.zip","filename":"a.zip","queued":true}"#,
    );
    loop {
        let msg = read_frame(&mut second);
        match msg["type"].as_str() {
            Some("error") => warnings += 1,
            Some("accepted") => break,
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }
    drop(second.stdin.take());
    assert!(second.wait().unwrap().success());
    assert_eq!(
        warnings, 1,
        "el aviso de «no guardo la cola» debe darse una sola vez"
    );

    drop(first.stdin.take());
    assert!(first.wait().unwrap().success());
    let after = std::fs::read(dir.path().join(".w11dl-queue.json")).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&after).contains("a.zip"),
        false,
        "el segundo host no debe haber escrito su cola"
    );
    let _ = before;
}
