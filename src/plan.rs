//! Planificación de fragmentos y estado de reanudación. Sin E/S, para poder probarlo aislado.

use serde::{Deserialize, Serialize};

/// Conexiones con las que se empieza. Desde aquí el controlador duplica mientras la velocidad suba.
pub const START_CONNECTIONS: usize = 2;
/// Ganancia mínima de velocidad total para seguir creciendo (10 %).
pub const RAMP_GAIN: f64 = 1.10;
/// Caída de velocidad que indica sobrecarga (el servidor tiene tope global): se retrocede.
pub const DROP_BELOW: f64 = 0.90;
/// Muestras sin mejora seguidas antes de quedarse en el mejor punto.
pub const FLAT_SAMPLES_TO_STOP: u32 = 3;
/// Muestras que se descartan tras añadir conexiones: las nuevas aún se están estableciendo.
pub const POST_GROWTH_SKIP: u32 = 1;
/// Caídas seguidas necesarias para retroceder. Una sola muestra baja puede ser ruido.
pub const DROPS_TO_RETREAT: u32 = 2;

/// Qué hacer tras una muestra de velocidad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Mantener el número actual de conexiones.
    Hold,
    /// Usar exactamente este número de conexiones (puede ser mayor o menor).
    Set(usize),
}

/// Controlador de conexiones. Busca el número de conexiones con mayor velocidad total:
/// - Arranque exponencial: duplica mientras la velocidad suba al menos `RAMP_GAIN`.
/// - Si la velocidad cae por debajo de `DROP_BELOW` del mejor punto, retrocede a él y para.
/// - Si tres muestras seguidas no mejoran, se queda en el mejor punto.
///
/// Es una máquina de estados pura: no mide nada por sí misma, solo decide con las muestras.
#[derive(Debug, Clone)]
pub struct Controller {
    max: usize,
    conns: usize,
    /// Mejor (conexiones, velocidad) observada hasta ahora.
    best: Option<(usize, f64)>,
    flat: u32,
    /// Caídas seguidas por debajo de `DROP_BELOW` del mejor punto.
    drops: u32,
    /// Muestras pendientes de descartar tras el último crecimiento.
    skip: u32,
    settled: bool,
    /// La primera muestra incluye el arranque de las conexiones: se descarta.
    warmed_up: bool,
}

impl Controller {
    pub fn new(max: usize) -> Self {
        let max = max.max(1);
        Self {
            max,
            conns: START_CONNECTIONS.min(max),
            best: None,
            flat: 0,
            drops: 0,
            skip: 0,
            settled: false,
            warmed_up: false,
        }
    }

    /// Conexiones que deben estar activas ahora.
    pub fn conns(&self) -> usize {
        self.conns
    }

    /// `true` cuando el controlador ya no va a cambiar el número de conexiones.
    pub fn settled(&self) -> bool {
        self.settled
    }

    /// Procesa una muestra de velocidad total (bytes/s) medida con el número actual de conexiones.
    pub fn observe(&mut self, rate: f64) -> Step {
        if self.settled || rate <= 0.0 {
            return Step::Hold;
        }
        if !self.warmed_up {
            self.warmed_up = true;
            return Step::Hold;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return Step::Hold;
        }
        let Some((best_conns, best_rate)) = self.best else {
            self.best = Some((self.conns, rate));
            return self.grow();
        };
        if rate >= best_rate * RAMP_GAIN {
            self.best = Some((self.conns, rate));
            self.flat = 0;
            self.drops = 0;
            return self.grow();
        }
        if rate < best_rate * DROP_BELOW {
            // Una caída aislada puede ser ruido: solo se retrocede si se confirma.
            self.drops += 1;
            if self.drops >= DROPS_TO_RETREAT {
                return self.settle_at(best_conns);
            }
            return Step::Hold;
        }
        self.drops = 0;
        self.flat += 1;
        if self.flat >= FLAT_SAMPLES_TO_STOP {
            return self.settle_at(best_conns);
        }
        Step::Hold
    }

    fn grow(&mut self) -> Step {
        if self.conns >= self.max {
            self.settled = true;
            return Step::Hold;
        }
        self.conns = (self.conns * 2).min(self.max);
        self.skip = POST_GROWTH_SKIP;
        Step::Set(self.conns)
    }

    fn settle_at(&mut self, conns: usize) -> Step {
        self.settled = true;
        if conns == self.conns {
            return Step::Hold;
        }
        self.conns = conns;
        Step::Set(conns)
    }
}

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

    /// Simula el controlador contra una curva de velocidad en función de las conexiones.
    fn run(curve: impl Fn(usize) -> f64, max: usize) -> (usize, usize) {
        let mut c = Controller::new(max);
        let mut peak = c.conns();
        for _ in 0..30 {
            if c.settled() {
                break;
            }
            match c.observe(curve(c.conns())) {
                Step::Set(n) => {
                    c.conns = n;
                    peak = peak.max(n);
                }
                Step::Hold => {}
            }
        }
        (c.conns(), peak)
    }

    #[test]
    fn limite_por_conexion_llega_al_maximo() {
        // 5 MB/s por conexión: cada conexión añade velocidad.
        let (conns, _) = run(|n| n as f64 * 5.0e6, 8);
        assert_eq!(conns, 8);
    }

    #[test]
    fn limite_global_no_sobrecarga_y_se_queda_en_el_minimo_util() {
        // 10 MB/s en total: a partir de 2 conexiones no hay ganancia.
        let (conns, peak) = run(|n| (n as f64 * 5.0e6).min(10.0e6), 8);
        assert_eq!(conns, 2, "debe retroceder al mejor punto");
        assert!(peak <= 4, "no debe llegar a 8 conexiones: pico {peak}");
    }

    #[test]
    fn velocidad_que_cae_hace_retroceder_al_mejor_punto() {
        // Sube hasta 4 conexiones y luego cae por sobrecarga.
        let curve = |n: usize| if n <= 4 { n as f64 * 5.0e6 } else { 12.0e6 };
        let (conns, _) = run(curve, 8);
        assert_eq!(conns, 4);
    }

    #[test]
    fn la_primera_muestra_es_calentamiento_y_no_decide() {
        let mut c = Controller::new(8);
        // Velocidad artificialmente baja al arrancar: no debe contar como referencia.
        assert_eq!(c.observe(1.0e6), Step::Hold);
        assert_eq!(c.observe(10.0e6), Step::Set(4));
    }

    #[test]
    fn el_falso_retroceso_de_la_traza_lenta_ya_no_ocurre() {
        // Secuencia real de la ejecución lenta: tras crecer a 8 conexiones, la primera muestra
        // mostró 8,4 MB/s (las conexiones nuevas aún no aportaban). Antes provocaba `Set(4)`.
        let mut c = Controller::new(8);
        assert_eq!(c.observe(8.4e6), Step::Hold, "calentamiento");
        assert_eq!(c.observe(8.4e6), Step::Set(4), "crece a 4");
        assert_eq!(
            c.observe(16.8e6),
            Step::Hold,
            "se descarta la muestra tras crecer"
        );
        assert_eq!(c.observe(16.8e6), Step::Set(8), "crece a 8");
        assert_eq!(
            c.observe(8.4e6),
            Step::Hold,
            "muestra artefacto tras crecer: se descarta"
        );
        assert_eq!(
            c.observe(33.0e6),
            Step::Hold,
            "ya con las 8 conexiones aportando"
        );
        assert!(c.conns() == 8, "no debe haber retrocedido: {}", c.conns());
    }

    #[test]
    fn una_sola_caida_no_basta_para_retroceder() {
        let mut c = Controller::new(8);
        c.observe(10.0e6); // calentamiento
        assert_eq!(c.observe(10.0e6), Step::Set(4));
        c.observe(20.0e6); // descartada tras crecer
        assert_eq!(c.observe(20.0e6), Step::Set(8));
        c.observe(5.0e6); // descartada tras crecer
        assert_eq!(
            c.observe(12.0e6),
            Step::Hold,
            "primera caída real: se espera confirmación"
        );
        assert_eq!(
            c.observe(40.0e6),
            Step::Hold,
            "se recupera: la caída no se confirmó"
        );
        assert_eq!(c.conns(), 8);
    }

    #[test]
    fn dos_caidas_seguidas_si_hacen_retroceder() {
        let mut c = Controller::new(8);
        c.observe(10.0e6);
        assert_eq!(c.observe(10.0e6), Step::Set(4));
        c.observe(20.0e6);
        assert_eq!(c.observe(20.0e6), Step::Set(8));
        c.observe(5.0e6);
        assert_eq!(c.observe(12.0e6), Step::Hold);
        assert_eq!(
            c.observe(12.0e6),
            Step::Set(4),
            "caída confirmada: vuelve al mejor punto"
        );
    }

    #[test]
    fn muestras_sin_datos_no_cambian_nada() {
        let mut c = Controller::new(8);
        assert_eq!(c.observe(0.0), Step::Hold);
        assert_eq!(c.conns(), START_CONNECTIONS);
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
