//! Cola de descargas del host: estado de cada descarga, control (pausa y cancelación) y persistencia.
//!
//! Se guarda en disco solo lo necesario para retomar: URL, ruta y estado. Las cabeceras de sesión
//! (cookies) se quedan en memoria: no se escriben nunca en disco.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hyper::header::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::engine::Control;

/// Archivo de estado de la cola, junto a las descargas.
pub const QUEUE_FILE: &str = ".w11dl-queue.json";

/// Estado de una descarga.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Running,
    /// En espera: no empieza hasta que el usuario la inicie o se ejecute la cola programada.
    Queued,
    Paused,
    /// Estaba en curso cuando el host se cerró. Se puede reanudar.
    Interrupted,
    Done,
    Canceled,
    Failed {
        message: String,
    },
}

impl State {
    /// Si la descarga puede reanudarse con `resume`.
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            State::Queued | State::Paused | State::Interrupted | State::Failed { .. }
        )
    }
}

/// Lo que se persiste y se muestra en la lista.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub url: String,
    pub path: PathBuf,
    #[serde(flatten)]
    pub state: State,
    pub bytes: u64,
    /// La descarga la hace yt-dlp desde la página (HLS, DASH…): `url` es la página, no un archivo.
    /// Se persiste para poder retomarla tras reiniciar, porque yt-dlp vuelve a resolver el enlace.
    #[serde(default)]
    pub via_ytdlp: bool,
}

/// Entrada en memoria: el registro, el control activo y las cabeceras de sesión (solo en memoria).
#[derive(Debug, Clone)]
pub struct Entry {
    pub record: Record,
    pub control: Control,
    pub headers: HeaderMap,
    /// Pistas separadas de un vídeo que se une con ffmpeg. Solo en memoria, como las cabeceras:
    /// los enlaces firmados caducan, así que no se guardan en disco.
    pub merge: Option<crate::merge::MergeJob>,
}

/// Cola compartida por las tareas del host.
#[derive(Clone, Default)]
pub struct Registry(Arc<Inner>);

#[derive(Default)]
struct Inner {
    shared: Mutex<Shared>,
    /// Un solo guardado a la vez: comparten archivo temporal y el más reciente debe ser el último.
    save_lock: Mutex<()>,
}

#[derive(Default)]
struct Shared {
    entries: Vec<Entry>,
    /// Destinos ya adjudicados cuya descarga aún no se ha registrado.
    reserved: HashSet<PathBuf>,
}

impl Registry {
    /// Carga la cola guardada. Lo que estaba en curso queda como `Interrupted`; lo pausado sigue pausado.
    pub fn load(dir: &Path) -> Self {
        let text = std::fs::read_to_string(dir.join(QUEUE_FILE)).unwrap_or_default();
        let records: Vec<Record> = serde_json::from_str(&text).unwrap_or_default();
        let entries = records
            .into_iter()
            .map(|mut record| {
                // Solo lo que corría queda interrumpido; una pausa del usuario sigue siendo una pausa.
                if record.state == State::Running {
                    record.state = State::Interrupted;
                }
                Entry {
                    record,
                    control: Control::default(),
                    headers: HeaderMap::new(),
                    merge: None,
                }
            })
            .collect();
        Self(Arc::new(Inner {
            shared: Mutex::new(Shared {
                entries,
                reserved: HashSet::new(),
            }),
            save_lock: Mutex::new(()),
        }))
    }

    /// Adjudica un destino libre en `dir` y lo reserva hasta que se registre la descarga.
    ///
    /// Un nombre está ocupado si existe el archivo, su parcial o su estado, si otra descarga de la
    /// cola (en espera, en curso, pausada o terminada) lo tiene, o si ya se adjudicó a otra
    /// petición. `also_exts` añade extensiones que el mismo nombre podría acabar teniendo (el
    /// contenedor de un vídeo lo decide yt-dlp). Comprobar y reservar es una sola operación: dos
    /// peticiones simultáneas nunca reciben el mismo destino.
    pub fn claim_path(&self, dir: &Path, filename: &str, also_exts: &[&str]) -> PathBuf {
        let mut shared = self.lock();
        let taken = |shared: &Shared, candidate: &Path| {
            let on_disk = |p: &Path| {
                p.exists()
                    || crate::naming::with_suffix(p, ".part").exists()
                    || crate::naming::with_suffix(p, ".part.state").exists()
            };
            let held = |p: &Path| {
                shared.reserved.contains(p)
                    || shared
                        .entries
                        .iter()
                        .any(|e| e.record.path == p && e.record.state != State::Canceled)
            };
            std::iter::once(candidate.to_path_buf())
                .chain(also_exts.iter().map(|e| candidate.with_extension(e)))
                .any(|p| on_disk(&p) || held(&p))
        };
        let mut n = 1;
        let path = loop {
            let candidate = dir.join(crate::naming::numbered(filename, n));
            if !taken(&shared, &candidate) {
                break candidate;
            }
            n += 1;
        };
        shared.reserved.insert(path.clone());
        path
    }

    /// Devuelve un destino reservado que al final no se usó.
    pub fn release_path(&self, path: &Path) {
        self.lock().reserved.remove(path);
    }

    /// Añade una descarga en curso y devuelve su control.
    pub fn insert(&self, record: Record, headers: HeaderMap) -> Control {
        let control = Control::default();
        let entry = Entry {
            record,
            control: control.clone(),
            headers,
            merge: None,
        };
        let mut shared = self.lock();
        shared.reserved.remove(&entry.record.path);
        shared.entries.push(entry);
        control
    }

    /// Modifica el registro de una descarga. Devuelve `None` si no existe.
    pub fn update<T>(&self, id: &str, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
        let mut shared = self.lock();
        shared.entries.iter_mut().find(|e| e.record.id == id).map(f)
    }

    pub fn get(&self, id: &str) -> Option<Entry> {
        self.lock()
            .entries
            .iter()
            .find(|e| e.record.id == id)
            .cloned()
    }

    pub fn records(&self) -> Vec<Record> {
        self.lock()
            .entries
            .iter()
            .map(|e| e.record.clone())
            .collect()
    }

    /// Guarda la cola en disco de forma atómica (archivo temporal y renombrado).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        // El candado cubre captura, escritura y renombrado: la instantánea más reciente es la última.
        let _saving = self
            .0
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let json = serde_json::to_vec_pretty(&self.records()).map_err(std::io::Error::other)?;
        let target = dir.join(QUEUE_FILE);
        let tmp = dir.join(format!("{QUEUE_FILE}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &target)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, state: State) -> Record {
        Record {
            id: id.into(),
            url: format!("https://x.test/{id}"),
            path: PathBuf::from(id),
            state,
            bytes: 7,
            via_ytdlp: false,
        }
    }

    #[test]
    fn al_cargar_lo_en_curso_queda_interrumpido_y_lo_pausado_sigue_pausado() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        reg.insert(record("a", State::Running), HeaderMap::new());
        reg.insert(record("b", State::Paused), HeaderMap::new());
        reg.insert(record("c", State::Done), HeaderMap::new());
        reg.save(dir.path()).unwrap();

        let loaded = Registry::load(dir.path());
        let states: Vec<State> = loaded.records().into_iter().map(|r| r.state).collect();
        assert_eq!(states, vec![State::Interrupted, State::Paused, State::Done]);
    }

    #[test]
    fn las_cabeceras_de_sesion_nunca_van_a_disco() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "sid=secreto".parse().unwrap());
        reg.insert(record("a", State::Running), headers);
        reg.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(QUEUE_FILE)).unwrap();
        assert!(!text.contains("secreto") && !text.to_lowercase().contains("cookie"));
    }

    #[test]
    fn estados_reanudables() {
        assert!(State::Paused.is_resumable());
        assert!(State::Queued.is_resumable());
        assert!(State::Interrupted.is_resumable());
        assert!(!State::Running.is_resumable());
        assert!(!State::Done.is_resumable());
    }

    #[test]
    fn guardados_simultaneos_no_se_pisan() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        for i in 0..20 {
            reg.insert(record(&format!("id{i}"), State::Running), HeaderMap::new());
        }
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (reg, path) = (reg.clone(), dir.path().to_path_buf());
                std::thread::spawn(move || {
                    (0..60)
                        .map(|_| reg.save(&path))
                        .filter(|r| r.is_err())
                        .count()
                })
            })
            .collect();
        let failures: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(failures, 0, "guardados fallidos por pisarse");
        let text = std::fs::read_to_string(dir.path().join(QUEUE_FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Record>>(&text).unwrap().len(),
            20
        );
    }

    #[test]
    fn update_sobre_id_inexistente_no_hace_nada() {
        let reg = Registry::default();
        assert!(reg.update("nope", |_| ()).is_none());
    }
}
