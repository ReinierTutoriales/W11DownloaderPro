//! Límite de velocidad: un cubo de fichas (bytes) que se rellena a `rate` por segundo.
//!
//! Se comparte entre todas las conexiones de una descarga, o entre varias descargas si se reparte
//! el mismo `Arc`. Cada bloque recibido pasa por `take`, que dice cuánto esperar antes de leer más:
//! dejar de leer llena la ventana TCP y es el propio servidor quien reduce su envío.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// El cubo guarda como mucho este tiempo de velocidad: un periodo de calma no compra una ráfaga
/// grande por encima del límite.
const BURST: f64 = 0.25;

struct Bucket {
    tokens: f64,
    at: Instant,
}

pub struct RateLimiter {
    /// Bytes por segundo; 0 significa sin límite. Cambiable en caliente.
    rate: Mutex<u64>,
    bucket: Mutex<Bucket>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RateLimiter({} B/s)", self.rate())
    }
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        Arc::new(Self {
            rate: Mutex::new(bytes_per_sec),
            bucket: Mutex::new(Bucket {
                tokens: 0.0,
                at: Instant::now(),
            }),
        })
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        *self.rate.lock().unwrap_or_else(|e| e.into_inner()) = bytes_per_sec;
    }

    pub fn rate(&self) -> u64 {
        *self.rate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Descuenta `bytes` y devuelve cuánto esperar. Superar el cubo deja deuda, y la deuda es la espera.
    pub fn take(&self, bytes: u64) -> Duration {
        self.take_at(bytes, Instant::now())
    }

    fn take_at(&self, bytes: u64, now: Instant) -> Duration {
        let rate = self.rate() as f64;
        if rate <= 0.0 {
            return Duration::ZERO;
        }
        let mut b = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
        let refill = now.saturating_duration_since(b.at).as_secs_f64() * rate;
        b.tokens = (b.tokens + refill).min(rate * BURST);
        b.at = now.max(b.at);
        b.tokens -= bytes as f64;
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / rate)
        }
    }

    /// Espera lo necesario tras recibir `bytes`.
    pub async fn throttle(&self, bytes: u64) {
        let wait = self.take(bytes);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Interpreta una velocidad como `500K`, `2M`, `1.5M` o bytes sueltos (`750000`). Base 1024.
pub fn parse_rate(text: &str) -> Option<u64> {
    let t = text.trim();
    let (num, mult) = match t.chars().last()? {
        'k' | 'K' => (&t[..t.len() - 1], 1024.0),
        'm' | 'M' => (&t[..t.len() - 1], 1024.0 * 1024.0),
        'g' | 'G' => (&t[..t.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (t, 1.0),
    };
    let value: f64 = num.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some((value * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_limite_no_espera() {
        let l = RateLimiter::new(0);
        assert_eq!(l.take(10 << 20), Duration::ZERO);
    }

    #[test]
    fn la_espera_acumulada_equivale_al_limite() {
        let l = RateLimiter::new(1_000_000);
        let t0 = Instant::now();
        // 10 bloques de 100 kB pedidos a la vez: 1 MB a 1 MB/s = ~1 s en total, sin ráfaga inicial.
        let mut wait = Duration::ZERO;
        for _ in 0..10 {
            wait = l.take_at(100_000, t0);
        }
        assert!((wait.as_secs_f64() - 1.0).abs() < 0.01, "{wait:?}");
    }

    #[test]
    fn la_calma_solo_compra_una_rafaga_corta() {
        let l = RateLimiter::new(1_000_000);
        let t0 = Instant::now();
        l.take_at(0, t0);
        // Tras 10 s en calma el cubo guarda 0,25 s de velocidad (250 kB), no 10 s.
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(l.take_at(250_000, t1), Duration::ZERO);
        assert!(l.take_at(250_000, t1) > Duration::from_millis(240));
    }

    #[test]
    fn el_limite_se_cambia_en_caliente() {
        let l = RateLimiter::new(1_000_000);
        l.set_rate(0);
        assert_eq!(l.take(5 << 20), Duration::ZERO);
        l.set_rate(500_000);
        assert_eq!(l.rate(), 500_000);
    }

    #[test]
    fn interpreta_velocidades() {
        assert_eq!(parse_rate("500K"), Some(512_000));
        assert_eq!(parse_rate("2M"), Some(2 << 20));
        assert_eq!(parse_rate("1.5m"), Some(1_572_864));
        assert_eq!(parse_rate("750000"), Some(750_000));
        assert_eq!(parse_rate("0"), Some(0));
        assert_eq!(parse_rate("abc"), None);
        assert_eq!(parse_rate("-1M"), None);
        assert_eq!(parse_rate(""), None);
    }
}
