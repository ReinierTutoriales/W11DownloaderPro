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

/// Estado compartido: la velocidad y el instante en que se habrá pagado toda la deuda de bytes
/// ya concedidos. Es el cubo de fichas expresado en tiempo: más sencillo de reescalar al cambiar la
/// velocidad en caliente sin perder la contabilidad de lo ya repartido.
struct Account {
    /// Bytes por segundo; 0 significa sin límite.
    rate: u64,
    paid_at: Instant,
}

pub struct RateLimiter {
    account: Mutex<Account>,
    /// Se avisa a las esperas en curso cada vez que cambia la velocidad.
    changed: tokio::sync::watch::Sender<u64>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RateLimiter({} B/s)", self.rate())
    }
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        Arc::new(Self {
            account: Mutex::new(Account {
                rate: bytes_per_sec,
                paid_at: Instant::now(),
            }),
            changed: tokio::sync::watch::channel(0).0,
        })
    }

    /// Cambia la velocidad. La deuda pendiente (bytes ya concedidos) se conserva y se reescala a la
    /// nueva velocidad, y las esperas en curso la recalculan de inmediato.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        let now = Instant::now();
        {
            let mut a = self.lock();
            if bytes_per_sec == 0 {
                a.paid_at = now;
            } else if a.rate > 0 {
                let owed = a.paid_at.saturating_duration_since(now).as_secs_f64() * a.rate as f64;
                a.paid_at = now + Duration::from_secs_f64(owed / bytes_per_sec as f64);
            }
            a.rate = bytes_per_sec;
        }
        self.changed.send_modify(|generation| *generation += 1);
    }

    pub fn rate(&self) -> u64 {
        self.lock().rate
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Account> {
        self.account.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Descuenta `bytes` y devuelve cuánto esperar. Superar la ráfaga deja deuda, y la deuda es la espera.
    pub fn take(&self, bytes: u64) -> Duration {
        self.take_at(bytes, Instant::now())
    }

    fn take_at(&self, bytes: u64, now: Instant) -> Duration {
        self.charge(bytes, now)
            .map_or(Duration::ZERO, |(deadline, _)| {
                deadline.saturating_duration_since(now)
            })
    }

    /// Concede `bytes`: devuelve el instante en que quedan pagados y la velocidad usada.
    fn charge(&self, bytes: u64, now: Instant) -> Option<(Instant, u64)> {
        let mut a = self.lock();
        if a.rate == 0 {
            return None;
        }
        // Un periodo de calma compra como mucho `BURST` segundos de velocidad.
        let floor = now
            .checked_sub(Duration::from_secs_f64(BURST))
            .unwrap_or(now);
        let start = a.paid_at.max(floor);
        a.paid_at = start + Duration::from_secs_f64(bytes as f64 / a.rate as f64);
        Some((a.paid_at, a.rate))
    }

    /// Espera lo necesario tras recibir `bytes`. Si la velocidad cambia mientras tanto, la espera
    /// pendiente se recalcula (quitar el límite la corta en el acto).
    pub async fn throttle(&self, bytes: u64) {
        let mut changes = self.changed.subscribe();
        let Some((mut deadline, mut rate)) = self.charge(bytes, Instant::now()) else {
            return;
        };
        loop {
            if deadline <= Instant::now() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.into()) => return,
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let now = self.rate();
                    if now == 0 {
                        return;
                    }
                    if now != rate {
                        let owed = deadline.saturating_duration_since(Instant::now()).as_secs_f64()
                            * rate as f64;
                        deadline = Instant::now() + Duration::from_secs_f64(owed / now as f64);
                        rate = now;
                    }
                }
            }
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
    fn quitar_el_limite_o_subirlo_corta_la_espera_en_curso() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            // 64 KiB a 1 KiB/s son unos 64 s de espera.
            let l = RateLimiter::new(1024);
            let waiting = {
                let l = Arc::clone(&l);
                tokio::spawn(async move { l.throttle(64 * 1024).await })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!waiting.is_finished());
            let t = Instant::now();
            l.set_rate(0);
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .expect("quitar el límite debe cortar la espera")
                .unwrap();
            assert!(t.elapsed() < Duration::from_secs(2));

            // Subirlo reescala la deuda: 64 KiB pendientes a 64 KiB/s tardan ~1 s, no ~64 s.
            let l = RateLimiter::new(1024);
            let waiting = {
                let l = Arc::clone(&l);
                tokio::spawn(async move { l.throttle(64 * 1024).await })
            };
            tokio::time::sleep(Duration::from_millis(100)).await;
            let t = Instant::now();
            l.set_rate(64 * 1024);
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .expect("subir el límite debe acortar la espera")
                .unwrap();
            assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        });
    }

    #[test]
    fn la_deuda_pendiente_sobrevive_al_cambio_de_velocidad() {
        let l = RateLimiter::new(1_000_000);
        let now = Instant::now();
        // 1 MB concedido a 1 MB/s: ~1 s de deuda. Al duplicar la velocidad queda ~0,5 s.
        let wait = l.take_at(1_000_000, now);
        assert!((wait.as_secs_f64() - 1.0).abs() < 0.05, "{wait:?}");
        l.set_rate(2_000_000);
        let after = l.take_at(0, Instant::now());
        assert!(after < Duration::from_millis(550), "{after:?}");
        assert!(after > Duration::from_millis(300), "{after:?}");
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
