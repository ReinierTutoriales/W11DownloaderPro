//! Planificación de fragmentos y estado de reanudación. Sin E/S, para poder probarlo aislado.

use serde::{Deserialize, Serialize};

/// Tamaño mínimo de fragmento: por debajo de esto el coste de cada petición domina.
pub const MIN_CHUNK: u64 = 64 * 1024;

/// Rango inclusivo de bytes `[start, end]`, como en la cabecera `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub index: usize,
    pub start: u64,
    pub end: u64,
}

impl Chunk {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// Tamaño de fragmento que da al menos cuatro fragmentos por conexión,
/// para que el reparto dinámico tenga margen de equilibrio, sin bajar de `MIN_CHUNK`.
pub fn effective_chunk_size(total: u64, connections: usize, max_chunk: u64) -> u64 {
    let per_connection = total / (connections.max(1) as u64 * 4);
    per_connection.clamp(MIN_CHUNK, max_chunk.max(MIN_CHUNK))
}

/// Divide `[0, total)` en fragmentos de `chunk_size`, el último puede ser más corto.
pub fn plan(total: u64, chunk_size: u64) -> Vec<Chunk> {
    if total == 0 {
        return Vec::new();
    }
    let chunk_size = chunk_size.max(1);
    let count = total.div_ceil(chunk_size);
    (0..count)
        .map(|i| {
            let start = i * chunk_size;
            let end = (start + chunk_size).min(total) - 1;
            Chunk {
                index: i as usize,
                start,
                end,
            }
        })
        .collect()
}

/// Estado persistido junto al archivo parcial. Permite reanudar sin volver a bajar lo completado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub url: String,
    pub total: u64,
    pub chunk_size: u64,
    /// Validador `ETag` de la respuesta original. Si cambia, el estado ya no es válido.
    pub etag: Option<String>,
    pub done: Vec<bool>,
}

impl State {
    pub fn new(url: &str, total: u64, chunk_size: u64, etag: Option<String>) -> Self {
        let count = plan(total, chunk_size).len();
        Self {
            url: url.to_owned(),
            total,
            chunk_size,
            etag,
            done: vec![false; count],
        }
    }

    /// Indica si este estado sigue siendo válido para el recurso actual.
    pub fn matches(&self, url: &str, total: u64, etag: Option<&str>) -> bool {
        self.url == url
            && self.total == total
            && match (&self.etag, etag) {
                (Some(saved), Some(current)) => saved == current,
                // Sin validador no hay forma de comprobar el recurso: no se reanuda.
                _ => false,
            }
    }

    pub fn completed_bytes(&self) -> u64 {
        plan(self.total, self.chunk_size)
            .iter()
            .filter(|c| self.done[c.index])
            .map(Chunk::len)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_cubre_todo_sin_huecos_ni_solapes() {
        let total = 10_000_003;
        let chunks = plan(total, 1_048_576);
        let mut expected_start = 0;
        for c in &chunks {
            assert_eq!(c.start, expected_start);
            expected_start = c.end + 1;
        }
        assert_eq!(expected_start, total);
        assert_eq!(chunks.iter().map(Chunk::len).sum::<u64>(), total);
    }

    #[test]
    fn plan_archivo_vacio_no_tiene_fragmentos() {
        assert!(plan(0, 1024).is_empty());
    }

    #[test]
    fn tamano_efectivo_respeta_minimo_y_maximo() {
        assert_eq!(effective_chunk_size(1_000, 8, 4 << 20), MIN_CHUNK);
        assert_eq!(effective_chunk_size(u64::MAX / 2, 8, 4 << 20), 4 << 20);
    }

    #[test]
    fn estado_rechaza_recurso_cambiado_o_sin_validador() {
        let s = State::new("http://x/a", 100, 10, Some("\"v1\"".into()));
        assert!(s.matches("http://x/a", 100, Some("\"v1\"")));
        assert!(!s.matches("http://x/a", 100, Some("\"v2\"")));
        assert!(!s.matches("http://x/a", 101, Some("\"v1\"")));
        assert!(!s.matches("http://x/b", 100, Some("\"v1\"")));
        assert!(!s.matches("http://x/a", 100, None));
    }

    #[test]
    fn bytes_completados_cuentan_solo_fragmentos_marcados() {
        let mut s = State::new("http://x/a", 25, 10, None);
        s.done[0] = true;
        s.done[2] = true;
        assert_eq!(s.completed_bytes(), 10 + 5);
    }
}
