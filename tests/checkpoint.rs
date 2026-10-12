//! Muchos fragmentos terminando a la vez no deben pisarse al guardar el estado de reanudación.
mod ranged;

use std::sync::Arc;
use std::time::Duration;

use ranged::{payload, ranged};
use w11dl::{Options, download};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpoints_simultaneos_no_se_pisan() {
    let data = payload(48 << 20);
    let addr = ranged(Arc::new(data.clone()), Duration::ZERO, 0, 1).await;
    for round in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.bin");
        let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
        opts.connections = 8;
        opts.max_chunk = 512 * 1024;
        download(opts)
            .await
            .unwrap_or_else(|e| panic!("ronda {round}: {e}"));
        assert_eq!(std::fs::read(&out).unwrap(), data, "ronda {round}");
        assert!(!dir.path().join("c.bin.part.state").exists());
    }
}
