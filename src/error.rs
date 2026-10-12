use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("URL no válida: {0}")]
    InvalidUrl(String),

    #[error("esquema no soportado: {0} (solo http y https)")]
    UnsupportedScheme(String),

    #[error("demasiadas redirecciones")]
    TooManyRedirects,

    #[error("el servidor respondió {0}")]
    Status(u16),

    #[error("el servidor ignoró la petición de rango (respondió {0} en lugar de 206)")]
    RangeIgnored(u16),

    #[error("cabecera Content-Range inconsistente: {0}")]
    BadContentRange(String),

    #[error("fragmento truncado: se esperaban {expected} bytes y llegaron {received}")]
    Truncated { expected: u64, received: u64 },

    #[error("error de conexión: {0}")]
    Connection(#[from] hyper_util::client::legacy::Error),

    #[error("error al leer la respuesta: {0}")]
    Body(#[from] hyper::Error),

    #[error("error de E/S: {0}")]
    Io(#[from] std::io::Error),

    #[error("el estado de reanudación no es válido: {0}")]
    CorruptState(#[from] serde_json::Error),

    #[error("hash SHA-256 no coincide: esperado {expected}, obtenido {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("el fragmento {index} falló tras {attempts} intentos: {source}")]
    ChunkFailed {
        index: usize,
        attempts: u32,
        #[source]
        source: Box<Error>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
