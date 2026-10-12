//! Planificación de fragmentos y estado de reanudación. Sin E/S, para poder probarlo aislado.

use serde::{Deserialize, Serialize};

/// Conexiones con las que se empieza. Desde aquí el controlador duplica mientras la velocidad suba.
pub const START_CONNECTIONS: usize = 2;
/// Conexiones iniciales para archivos grandes, donde la rampa desde 2 es una parte cara de la descarga.
pub const START_CONNECTIONS_LARGE: usize = 4;
/// Tamaño a partir del cual un archivo se considera grande para elegir las conexiones iniciales.
pub const LARGE_FILE: u64 = 8 * 1024 * 1024;

/// Conexiones con las que empezar según el tamaño. Los archivos pequeños terminan antes de que
/// compense abrir más; los grandes ganan al saltarse parte de la rampa.
pub fn start_connections(total: u64, max: usize) -> usize {
    let start = if total >= LARGE_FILE {
        START_CONNECTIONS_LARGE
    } else {
        START_CONNECTIONS
    };
    start.min(max.max(1))
}

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
        Self::with_start(max, START_CONNECTIONS)
    }

    /// Controlador que arranca con `start` conexiones (acotadas por `max`).
    pub fn with_start(max: usize, start: usize) -> Self {
        let max = max.max(1);
        Self {
            max,
            conns: start.clamp(1, max),
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

/// Tiempo sin nuevos rechazos tras el cual el tope sube una conexión.
pub const CEILING_RECOVER_MS: u64 = 60_000;

/// Tope de conexiones que impone un servidor que rechaza algunas (503, 429 o 403) mientras sirve
/// datos por otras. Se fija en las conexiones que seguían sirviendo, y sube de una en una cada
/// `CEILING_RECOVER_MS` sin nuevos rechazos: un servidor que estuvo mal un momento no limita para
/// siempre una descarga larga, y uno que lo decía en serio cuesta un rechazo por minuto.
#[derive(Debug, Clone)]
pub struct Ceiling {
    max: usize,
    limit: Option<usize>,
    since_ms: u64,
}

impl Ceiling {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            limit: None,
            since_ms: 0,
        }
    }

    /// Un servidor rechazó una conexión mientras servía otras `serving`. El tope solo baja.
    pub fn on_refusal(&mut self, serving: usize, now_ms: u64) {
        let cap = serving.max(1).min(self.max);
        self.limit = Some(self.limit.map_or(cap, |l| l.min(cap)));
        self.since_ms = now_ms;
    }

    /// Sube el tope una conexión si pasó el tiempo de recuperación. Devuelve si cambió.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        let Some(limit) = self.limit else {
            return false;
        };
        if now_ms.saturating_sub(self.since_ms) < CEILING_RECOVER_MS {
            return false;
        }
        self.since_ms = now_ms;
        self.limit = if limit + 1 >= self.max {
            None
        } else {
            Some(limit + 1)
        };
        true
    }

    /// Conexiones permitidas ahora.
    pub fn cap(&self) -> usize {
        self.limit.unwrap_or(self.max)
    }

    pub fn is_limited(&self) -> bool {
        self.limit.is_some()
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

/// Los últimos fragmentos se parten en trozos de `chunk_size / TAPER_DIVISOR`.
const TAPER_DIVISOR: u64 = 4;
/// La cola afilada abarca los últimos `TAPER_TAIL_CHUNKS` fragmentos.
const TAPER_TAIL_CHUNKS: u64 = 4;
/// Solo se afila la cola si el archivo tiene al menos tantos fragmentos como esto.
const TAPER_MIN_CHUNKS: u64 = 8;

/// Divide `[0, total)` en fragmentos de `chunk_size`, con la cola partida en trozos menores.
///
/// Con fragmentos uniformes, al final unas conexiones esperan un fragmento entero (a la velocidad
/// de *una* conexión) mientras las demás están ociosas. Partir los últimos fragmentos reduce esa
/// espera. El plan es una función de `(total, chunk_size)`: el estado de reanudación depende de que
/// no cambie entre ejecuciones.
pub fn plan(total: u64, chunk_size: u64) -> Vec<Chunk> {
    if total == 0 {
        return Vec::new();
    }
    let chunk_size = chunk_size.max(1);
    let small = chunk_size / TAPER_DIVISOR;
    let taper = small >= MIN_CHUNK && total >= chunk_size * TAPER_MIN_CHUNKS;
    let tail = if taper {
        (chunk_size * TAPER_TAIL_CHUNKS).min(total / 4)
    } else {
        0
    };
    let body_end = total - tail;

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < body_end {
        let end = (start + chunk_size).min(body_end) - 1;
        chunks.push(Chunk {
            index: chunks.len(),
            start,
            end,
        });
        start = end + 1;
    }
    while start < total {
        let end = (start + small).min(total) - 1;
        chunks.push(Chunk {
            index: chunks.len(),
            start,
            end,
        });
        start = end + 1;
    }
    chunks
}

/// Validadores del recurso remoto: lo que permite saber si sigue siendo el mismo archivo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validators {
    pub fn new(etag: Option<String>, last_modified: Option<String>) -> Self {
        Self {
            etag,
            last_modified,
        }
    }

    /// Valor para `If-Range`. La RFC 9110 §13.1.5 solo admite validadores fuertes: un `ETag`
    /// débil (`W/"…"`) nunca coincide, así que se usa `Last-Modified` en su lugar.
    pub fn if_range(&self) -> Option<&str> {
        match self.etag.as_deref() {
            Some(tag) if !tag.starts_with("W/") => Some(tag),
            _ => self.last_modified.as_deref(),
        }
    }
}

/// `ETag` sin el prefijo débil ni espacios, para comparar el recurso (no para `If-Range`).
fn etag_core(tag: &str) -> &str {
    tag.trim().trim_start_matches("W/")
}

/// Estado persistido junto al archivo parcial. Permite reanudar sin volver a bajar lo completado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub url: String,
    pub total: u64,
    pub chunk_size: u64,
    /// Validador `ETag` de la respuesta original. Si cambia, el estado ya no es válido.
    pub etag: Option<String>,
    /// `Last-Modified` de la respuesta original. Estados antiguos no lo traen.
    #[serde(default)]
    pub last_modified: Option<String>,
    pub done: Vec<bool>,
}

impl State {
    pub fn new(url: &str, total: u64, chunk_size: u64, validators: Validators) -> Self {
        let count = plan(total, chunk_size).len();
        Self {
            url: url.to_owned(),
            total,
            chunk_size,
            etag: validators.etag,
            last_modified: validators.last_modified,
            done: vec![false; count],
        }
    }

    /// Indica si este estado sigue siendo válido para el recurso actual.
    ///
    /// El tamaño y la URL deben coincidir siempre (prueba). Cada validador presente en ambos
    /// lados debe coincidir (indicio): si cualquiera difiere, el archivo cambió. Si el servidor
    /// no ofrece validadores comparables, se reanuda por tamaño; con `--sha256` el resultado
    /// final se verifica igualmente.
    pub fn matches(&self, url: &str, total: u64, current: &Validators) -> bool {
        self.url == url
            && self.total == total
            && match (&self.etag, &current.etag) {
                (Some(a), Some(b)) => etag_core(a) == etag_core(b),
                _ => true,
            }
            && match (&self.last_modified, &current.last_modified) {
                (Some(a), Some(b)) => a.trim() == b.trim(),
                _ => true,
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
    fn el_arranque_depende_del_tamano() {
        assert_eq!(
            start_connections(1024, 8),
            START_CONNECTIONS,
            "archivo pequeño"
        );
        assert_eq!(start_connections(LARGE_FILE - 1, 8), START_CONNECTIONS);
        assert_eq!(
            start_connections(LARGE_FILE, 8),
            START_CONNECTIONS_LARGE,
            "archivo grande"
        );
        assert_eq!(
            start_connections(1 << 30, 2),
            2,
            "nunca supera el máximo permitido"
        );
        assert_eq!(start_connections(1 << 30, 0), 1, "máximo degenerado");
    }

    #[test]
    fn controlador_con_arranque_alto_crece_desde_ahi() {
        let mut c = Controller::with_start(8, 4);
        assert_eq!(c.conns(), 4);
        c.observe(20.0e6); // calentamiento
        assert_eq!(
            c.observe(20.0e6),
            Step::Set(8),
            "de 4 pasa directamente a 8"
        );
    }

    #[test]
    fn muestras_sin_datos_no_cambian_nada() {
        let mut c = Controller::new(8);
        assert_eq!(c.observe(0.0), Step::Hold);
        assert_eq!(c.conns(), START_CONNECTIONS);
    }

    #[test]
    fn la_cola_se_parte_en_trozos_menores() {
        let chunk = 2 * 1024 * 1024;
        let chunks = plan(64 * 1024 * 1024, chunk);
        let sizes: Vec<u64> = chunks.iter().map(Chunk::len).collect();
        assert_eq!(
            *sizes.first().unwrap(),
            chunk,
            "el cuerpo usa el tamaño normal"
        );
        assert_eq!(
            *sizes.last().unwrap(),
            chunk / 4,
            "la cola usa trozos menores"
        );
        assert!(
            chunks.len() > 32,
            "más fragmentos que el plan uniforme: {}",
            chunks.len()
        );
        // Los menores solo aparecen al final: nunca hay uno grande después de uno pequeño.
        let first_small = sizes.iter().position(|s| *s < chunk).unwrap();
        assert!(sizes[first_small..].iter().all(|s| *s <= chunk / 4));
    }

    #[test]
    fn el_plan_afilado_sigue_cubriendo_todo_sin_huecos() {
        for total in [3_000_000u64, 17_777_777, 64 * 1024 * 1024 + 123] {
            let chunks = plan(total, 1024 * 1024);
            let mut next = 0;
            for (i, c) in chunks.iter().enumerate() {
                assert_eq!(c.index, i);
                assert_eq!(c.start, next, "hueco o solape en {total}");
                next = c.end + 1;
            }
            assert_eq!(next, total);
        }
    }

    #[test]
    fn archivos_pequenos_o_con_fragmentos_minimos_no_se_afilan() {
        // Pocos fragmentos: no hay cola que afilar.
        assert_eq!(plan(3 * 1024 * 1024, 1024 * 1024).len(), 3);
        // Fragmento demasiado pequeño para dividirlo sin bajar de MIN_CHUNK.
        let chunks = plan(10_000_000, MIN_CHUNK);
        assert!(
            chunks
                .iter()
                .take(chunks.len() - 1)
                .all(|c| c.len() == MIN_CHUNK)
        );
    }

    #[test]
    fn el_tope_se_fija_en_las_conexiones_que_seguian_sirviendo() {
        let mut c = Ceiling::new(8);
        assert_eq!(c.cap(), 8);
        c.on_refusal(3, 1_000);
        assert_eq!(c.cap(), 3);
        c.on_refusal(2, 2_000);
        assert_eq!(c.cap(), 2, "un rechazo posterior solo baja el tope");
        c.on_refusal(5, 3_000);
        assert_eq!(c.cap(), 2, "nunca sube por un rechazo");
        c.on_refusal(0, 4_000);
        assert_eq!(c.cap(), 1, "mínimo una conexión");
    }

    #[test]
    fn el_tope_se_recupera_de_una_en_una_cada_minuto() {
        let mut c = Ceiling::new(4);
        c.on_refusal(1, 0);
        assert!(!c.tick(59_000), "aún no ha pasado el minuto");
        assert_eq!(c.cap(), 1);
        assert!(c.tick(60_000));
        assert_eq!(c.cap(), 2);
        assert!(!c.tick(90_000), "el minuto cuenta desde la última subida");
        assert!(c.tick(120_000));
        assert_eq!(c.cap(), 3);
        assert!(c.tick(180_000));
        assert!(!c.is_limited(), "al llegar al máximo el tope desaparece");
        assert_eq!(c.cap(), 4);
    }

    #[test]
    fn un_nuevo_rechazo_reinicia_la_espera_de_recuperacion() {
        let mut c = Ceiling::new(8);
        c.on_refusal(2, 0);
        c.on_refusal(2, 50_000);
        assert!(!c.tick(100_000), "el último rechazo fue hace 50 s");
        assert!(c.tick(110_000));
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

    fn v(etag: Option<&str>, lm: Option<&str>) -> Validators {
        Validators::new(etag.map(Into::into), lm.map(Into::into))
    }

    #[test]
    fn estado_rechaza_recurso_cambiado() {
        let s = State::new("http://x/a", 100, 10, v(Some("\"v1\""), Some("Mon")));
        assert!(s.matches("http://x/a", 100, &v(Some("\"v1\""), Some("Mon"))));
        assert!(!s.matches("http://x/a", 100, &v(Some("\"v2\""), Some("Mon"))));
        assert!(!s.matches("http://x/a", 100, &v(Some("\"v1\""), Some("Tue"))));
        assert!(!s.matches("http://x/a", 101, &v(Some("\"v1\""), Some("Mon"))));
        assert!(!s.matches("http://x/b", 100, &v(Some("\"v1\""), Some("Mon"))));
    }

    #[test]
    fn etag_debil_y_fuerte_del_mismo_recurso_coinciden() {
        let s = State::new("http://x/a", 100, 10, v(Some("W/\"v1\""), None));
        assert!(s.matches("http://x/a", 100, &v(Some("\"v1\""), None)));
        assert!(!s.matches("http://x/a", 100, &v(Some("W/\"v2\""), None)));
    }

    #[test]
    fn sin_validadores_comparables_se_reanuda_por_tamano() {
        let s = State::new("http://x/a", 100, 10, v(None, None));
        assert!(s.matches("http://x/a", 100, &v(None, None)));
        assert!(!s.matches("http://x/a", 99, &v(None, None)));
        // Solo Last-Modified en un lado y ETag en el otro: no hay nada comparable.
        let s = State::new("http://x/a", 100, 10, v(None, Some("Mon")));
        assert!(s.matches("http://x/a", 100, &v(Some("\"e\""), None)));
        assert!(!s.matches("http://x/a", 100, &v(None, Some("Tue"))));
    }

    #[test]
    fn if_range_usa_solo_validadores_fuertes() {
        assert_eq!(v(Some("\"a\""), Some("Mon")).if_range(), Some("\"a\""));
        assert_eq!(v(Some("W/\"a\""), Some("Mon")).if_range(), Some("Mon"));
        assert_eq!(v(Some("W/\"a\""), None).if_range(), None);
        assert_eq!(v(None, Some("Mon")).if_range(), Some("Mon"));
    }

    #[test]
    fn bytes_completados_cuentan_solo_fragmentos_marcados() {
        let mut s = State::new("http://x/a", 25, 10, Validators::default());
        s.done[0] = true;
        s.done[2] = true;
        assert_eq!(s.completed_bytes(), 10 + 5);
    }
}
