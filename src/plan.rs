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

/// Edad mínima de un fragmento en curso antes de plantearse una copia: antes, su velocidad aún no dice nada.
pub const HEDGE_MIN_AGE_MS: u64 = 250;
/// Coste de abrir una petición nueva antes del primer byte, que se suma al tiempo de una copia.
const HEDGE_STARTUP_SECS: f64 = 0.05;

/// ¿Compensa lanzar una copia de un fragmento que otra conexión sigue bajando?
///
/// Solo si la copia, que empieza de cero a la velocidad habitual, terminaría en menos de la mitad
/// del tiempo que le queda al original a su ritmo actual. Así el trabajo duplicado solo se paga
/// cuando casi seguro gana, y un fragmento que avanza bien nunca se duplica.
pub fn hedge_worthwhile(len: u64, received: u64, age_ms: u64, typical_rate: f64) -> bool {
    if age_ms < HEDGE_MIN_AGE_MS || typical_rate <= 0.0 || received >= len {
        return false;
    }
    let rate = received as f64 / (age_ms as f64 / 1000.0);
    let remaining = (len - received) as f64;
    let eta_original = if rate > 0.0 {
        remaining / rate
    } else {
        f64::INFINITY
    };
    let eta_copy = HEDGE_STARTUP_SECS + len as f64 / typical_rate;
    eta_original >= 2.0 * eta_copy
}

/// Segundos desde 1970 de una fecha HTTP en formato IMF (`Sun, 06 Nov 1994 08:49:37 GMT`), el único
/// que los servidores deben enviar hoy. La gramática es estricta: campos de ancho fijo y solo dígitos,
/// mes y día de la semana en inglés, día que exista en ese mes (con años bisiestos), día de la semana
/// que corresponda a la fecha, hora 00-23, minutos y segundos 00-59 y zona `GMT`. Cualquier otra forma
/// (RFC 850, asctime, valores fuera de rango) se trata como desconocida.
pub fn parse_http_date(text: &str) -> Option<i64> {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let b = text.trim().as_bytes();
    // "Www, DD Mmm YYYY HH:MM:SS GMT" -> 29 bytes con separadores en posiciones fijas.
    if b.len() != 29
        || &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..] != b" GMT"
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = &b[range];
        digits.iter().all(u8::is_ascii_digit).then(|| {
            digits
                .iter()
                .fold(0i64, |n, d| n * 10 + i64::from(d - b'0'))
        })
    };
    let weekday = DAYS.iter().position(|d| d.as_bytes() == &b[0..3])? as i64;
    let day = num(5..7)?;
    let month = MONTHS.iter().position(|m| m.as_bytes() == &b[8..11])? as i64 + 1;
    let year = num(12..16)?;
    let (h, m, sec) = (num(17..19)?, num(20..22)?, num(23..25)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => 28 + i64::from(leap),
    };
    if year < 1970 || !(1..=days_in_month).contains(&day) || h > 23 || m > 59 || sec > 59 {
        return None;
    }
    // Días desde 1970-01-01 (algoritmo de calendario civil de Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    // El 1 de enero de 1970 fue jueves (índice 3 con lunes = 0).
    if (days + 3).rem_euclid(7) != weekday {
        return None;
    }
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

/// Separación mínima entre `Date` y `Last-Modified` para fiarse de este como validador fuerte.
///
/// La RFC 9110 §8.8.2.2 pide al menos un segundo, pero da por supuesto que ambas fechas salen del
/// mismo reloj. Detrás de una CDN, `Date` lo pone el borde y `Last-Modified` el origen, con relojes
/// distintos. Un minuto absorbe desajustes normales; si hay duda, no se fía (se pierde la reanudación
/// sin SHA-256, nunca la integridad).
pub const LM_STRONG_MARGIN_SECS: i64 = 60;

/// Validadores del recurso remoto: lo que permite saber si sigue siendo el mismo archivo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// `Last-Modified` solo es un validador fuerte si el recurso se modificó al menos
    /// `LM_STRONG_MARGIN_SECS` antes de la fecha de la respuesta (RFC 9110 §8.8.2.2, con margen para
    /// relojes distintos entre origen y CDN). Sin `Date`, o con fechas ilegibles o futuras, no es fuerte.
    pub lm_strong: bool,
}

impl Validators {
    pub fn new(
        etag: Option<String>,
        last_modified: Option<String>,
        response_date: Option<&str>,
    ) -> Self {
        let lm_strong = match (
            last_modified.as_deref().and_then(parse_http_date),
            response_date.and_then(parse_http_date),
        ) {
            (Some(modified), Some(date)) => date - modified >= LM_STRONG_MARGIN_SECS,
            _ => false,
        };
        Self {
            etag,
            last_modified,
            lm_strong,
        }
    }

    /// Valor para `If-Range`. La RFC 9110 §13.1.5 solo admite validadores fuertes: un `ETag`
    /// débil (`W/"…"`) nunca coincide, y un `Last-Modified` solo cuenta si es fuerte.
    pub fn if_range(&self) -> Option<&str> {
        match self.etag.as_deref() {
            Some(tag) if !tag.starts_with("W/") => Some(tag),
            _ if self.lm_strong => self.last_modified.as_deref(),
            _ => None,
        }
    }
}

fn is_weak(tag: &str) -> bool {
    tag.trim().starts_with("W/")
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
    /// Si ese `Last-Modified` era un validador fuerte. Estados antiguos: no.
    #[serde(default)]
    pub lm_strong: bool,
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
            lm_strong: validators.lm_strong,
            done: vec![false; count],
        }
    }

    /// Indica si este estado sigue siendo válido para el recurso actual.
    ///
    /// La URL y el tamaño deben coincidir siempre. Si algún validador presente en ambos lados
    /// difiere, el archivo cambió. Para reanudar hace falta además una verificación del contenido:
    /// un `ETag` fuerte o un `Last-Modified` presentes en ambos lados y coincidentes. Un `ETag`
    /// débil no basta (solo promete equivalencia, no los mismos bytes). Sin esa verificación solo se
    /// reanuda con `allow_unverified`, que el motor activa cuando hay un SHA-256 esperado: el
    /// resultado final se comprueba y cualquier mezcla se detecta.
    pub fn matches(
        &self,
        url: &str,
        total: u64,
        current: &Validators,
        allow_unverified: bool,
    ) -> bool {
        if self.url != url || self.total != total {
            return false;
        }
        let etag = match (&self.etag, &current.etag) {
            (Some(a), Some(b)) => Some((etag_core(a) == etag_core(b), !is_weak(a) && !is_weak(b))),
            _ => None,
        };
        let modified = match (&self.last_modified, &current.last_modified) {
            (Some(a), Some(b)) => Some((a.trim() == b.trim(), self.lm_strong && current.lm_strong)),
            _ => None,
        };
        if etag.is_some_and(|(same, _)| !same) || modified.is_some_and(|(same, _)| !same) {
            return false;
        }
        let verified = etag == Some((true, true)) || modified == Some((true, true));
        verified || allow_unverified
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

    /// Validadores de prueba: un `Last-Modified` se considera fuerte.
    fn v(etag: Option<&str>, lm: Option<&str>) -> Validators {
        Validators {
            etag: etag.map(Into::into),
            last_modified: lm.map(Into::into),
            lm_strong: lm.is_some(),
        }
    }

    #[test]
    fn estado_rechaza_recurso_cambiado() {
        let s = State::new("http://x/a", 100, 10, v(Some("\"v1\""), Some("Mon")));
        assert!(s.matches("http://x/a", 100, &v(Some("\"v1\""), Some("Mon")), false));
        for strict in [false, true] {
            // Un validador distinto invalida el parcial incluso con `allow_unverified`.
            assert!(!s.matches("http://x/a", 100, &v(Some("\"v2\""), Some("Mon")), strict));
            assert!(!s.matches("http://x/a", 100, &v(Some("\"v1\""), Some("Tue")), strict));
            assert!(!s.matches("http://x/a", 101, &v(Some("\"v1\""), Some("Mon")), strict));
            assert!(!s.matches("http://x/b", 100, &v(Some("\"v1\""), Some("Mon")), strict));
        }
    }

    #[test]
    fn un_solo_validador_comparable_basta_si_es_fuerte() {
        // ETag fuerte solo, o Last-Modified solo.
        let s = State::new("http://x/a", 100, 10, v(Some("\"v1\""), None));
        assert!(s.matches("http://x/a", 100, &v(Some("\"v1\""), None), false));
        let s = State::new("http://x/a", 100, 10, v(None, Some("Mon")));
        assert!(s.matches("http://x/a", 100, &v(None, Some("Mon")), false));
        // El servidor añadió otro validador entre medias: el que se puede comparar coincide.
        assert!(s.matches("http://x/a", 100, &v(Some("\"e\""), Some("Mon")), false));
    }

    #[test]
    fn etag_debil_no_verifica_pero_distinto_invalida() {
        let s = State::new("http://x/a", 100, 10, v(Some("W/\"v1\""), None));
        assert!(
            !s.matches("http://x/a", 100, &v(Some("W/\"v1\""), None), false),
            "débil: no verifica"
        );
        assert!(
            !s.matches("http://x/a", 100, &v(Some("\"v1\""), None), false),
            "débil de un lado: no verifica"
        );
        assert!(
            s.matches("http://x/a", 100, &v(Some("W/\"v1\""), None), true),
            "con SHA-256 se permite"
        );
        assert!(
            !s.matches("http://x/a", 100, &v(Some("W/\"v2\""), None), true),
            "distinto invalida siempre"
        );
        // Débil más Last-Modified coincidente sí verifica.
        let s = State::new("http://x/a", 100, 10, v(Some("W/\"v1\""), Some("Mon")));
        assert!(s.matches("http://x/a", 100, &v(Some("W/\"v1\""), Some("Mon")), false));
    }

    #[test]
    fn sin_verificacion_solo_se_reanuda_si_se_permite() {
        let s = State::new("http://x/a", 100, 10, v(None, None));
        assert!(
            !s.matches("http://x/a", 100, &v(None, None), false),
            "sin validadores no se reanuda"
        );
        assert!(
            s.matches("http://x/a", 100, &v(None, None), true),
            "con SHA-256 sí"
        );
        assert!(
            !s.matches("http://x/a", 99, &v(None, None), true),
            "el tamaño siempre cuenta"
        );
        // Validadores de clase distinta en cada lado: nada comparable.
        let s = State::new("http://x/a", 100, 10, v(None, Some("Mon")));
        assert!(!s.matches("http://x/a", 100, &v(Some("\"e\""), None), false));
        assert!(s.matches("http://x/a", 100, &v(Some("\"e\""), None), true));
        assert!(!s.matches("http://x/a", 100, &v(None, Some("Tue")), true));
    }

    #[test]
    fn fechas_http() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Thu, 29 Feb 2024 12:00:00 GMT"),
            Some(1_709_208_000)
        );
        assert_eq!(
            parse_http_date("  Thu, 01 Jan 1970 00:00:00 GMT  "),
            Some(0),
            "espacios sobrantes"
        );
    }

    #[test]
    fn fechas_http_invalidas_se_rechazan() {
        for bad in [
            "",
            "ayer",
            "Sun Nov  6 08:49:37 1994",       // asctime
            "Sunday, 06-Nov-94 08:49:37 GMT", // RFC 850
            "Sun, 06 Nov 1994 08:49:37 UTC",  // zona
            "Sun, 32 Nov 1994 08:49:37 GMT",  // día inexistente
            "Wed, 31 Nov 1994 08:49:37 GMT",  // noviembre tiene 30 días
            "Tue, 30 Feb 1994 08:49:37 GMT",  // febrero
            "Fri, 29 Feb 2023 08:49:37 GMT",  // 2023 no es bisiesto
            "Sun, 06 Foo 1994 08:49:37 GMT",  // mes
            "Sun, 06 Nov 1994 25:49:37 GMT",  // hora
            "Sun, 06 Nov 1994 08:60:37 GMT",  // minutos
            "Sun, 06 Nov 1994 08:49:60 GMT",  // segundos (segundo intercalar)
            "Sun, 06 Nov 1994 -8:49:37 GMT",  // signo
            "Sun, 06 Nov 1994 +8:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",  // ancho variable
            "Sun, 06 Nov 94 08:49:37 GMT",   // año de dos cifras
            "Mon, 06 Nov 1994 08:49:37 GMT", // el día de la semana no corresponde
            "Sun, 06 Nov 1969 08:49:37 GMT", // antes de 1970
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn last_modified_es_fuerte_solo_con_margen_suficiente_respecto_a_date() {
        let lm = "Sun, 06 Nov 1994 08:49:37 GMT";
        let with = |date: Option<&str>| Validators::new(None, Some(lm.into()), date);
        assert!(
            with(Some("Sun, 06 Nov 1994 08:50:37 GMT")).lm_strong,
            "60 s: fuerte"
        );
        assert!(with(Some("Mon, 07 Nov 1994 08:49:37 GMT")).lm_strong);
        assert!(
            !with(Some("Sun, 06 Nov 1994 08:50:36 GMT")).lm_strong,
            "59 s: aún débil"
        );
        assert!(
            !with(Some("Sun, 06 Nov 1994 08:49:38 GMT")).lm_strong,
            "1 s: la RFC lo admitiría, aquí no"
        );
        assert!(
            !with(Some("Sun, 06 Nov 1994 08:49:37 GMT")).lm_strong,
            "mismo segundo"
        );
        assert!(
            !with(Some("Sun, 06 Nov 1994 08:49:36 GMT")).lm_strong,
            "modificado en el futuro"
        );
        assert!(!with(None).lm_strong, "sin Date no se puede saber");
        assert!(!with(Some("basura")).lm_strong);
        assert!(
            !with(Some("Sun, 31 Feb 1994 08:49:37 GMT")).lm_strong,
            "Date inválido"
        );
        assert!(
            !Validators::new(
                None,
                Some("basura".into()),
                Some("Mon, 07 Nov 1994 08:49:37 GMT")
            )
            .lm_strong
        );
    }

    #[test]
    fn last_modified_debil_no_sirve_ni_para_if_range_ni_para_verificar() {
        let weak = Validators::new(
            None,
            Some("Sun, 06 Nov 1994 08:49:37 GMT".into()),
            Some("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        assert_eq!(weak.if_range(), None);
        let s = State::new("http://x/a", 100, 10, weak.clone());
        assert!(!s.matches("http://x/a", 100, &weak, false), "no verifica");
        assert!(
            s.matches("http://x/a", 100, &weak, true),
            "con SHA-256 sí se permite"
        );
        let changed = Validators {
            last_modified: Some("Sun, 06 Nov 1994 08:49:40 GMT".into()),
            ..weak
        };
        assert!(
            !s.matches("http://x/a", 100, &changed, true),
            "distinto invalida siempre"
        );
    }

    #[test]
    fn if_range_usa_solo_validadores_fuertes() {
        assert_eq!(v(Some("\"a\""), Some("Mon")).if_range(), Some("\"a\""));
        assert_eq!(v(Some("W/\"a\""), Some("Mon")).if_range(), Some("Mon"));
        assert_eq!(v(Some("W/\"a\""), None).if_range(), None);
        assert_eq!(v(None, Some("Mon")).if_range(), Some("Mon"));
    }

    #[test]
    fn copia_solo_si_casi_seguro_gana() {
        let mib = 1 << 20;
        // Fragmento de 1 MiB con 100 KiB tras 1 s (~100 KiB/s) y velocidad habitual de 5 MiB/s: copiar.
        assert!(hedge_worthwhile(mib, 100 * 1024, 1000, 5.0 * mib as f64));
        // Fragmento de 4 MiB que avanza a la velocidad habitual: no se duplica.
        assert!(!hedge_worthwhile(
            4 * mib,
            5 * mib / 2,
            500,
            5.0 * mib as f64
        ));
        // Uno que va a la mitad de la habitual tampoco: la copia no ganaría por el doble.
        assert!(!hedge_worthwhile(
            4 * mib,
            5 * mib / 4,
            500,
            5.0 * mib as f64
        ));
        // Demasiado joven para medir, sin referencia de velocidad, o ya completo.
        assert!(!hedge_worthwhile(mib, 0, 100, 5.0 * mib as f64));
        assert!(!hedge_worthwhile(mib, 0, 5000, 0.0));
        assert!(!hedge_worthwhile(mib, mib, 5000, 5.0 * mib as f64));
        // Sin recibir nada pasado el mínimo: velocidad desconocida, tiempo infinito, se copia.
        assert!(hedge_worthwhile(mib, 0, 1000, 5.0 * mib as f64));
    }

    #[test]
    fn bytes_completados_cuentan_solo_fragmentos_marcados() {
        let mut s = State::new("http://x/a", 25, 10, Validators::default());
        s.done[0] = true;
        s.done[2] = true;
        assert_eq!(s.completed_bytes(), 10 + 5);
    }
}
