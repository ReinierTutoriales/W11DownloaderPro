//! Motor de descargas de W11DownloaderPro.

pub mod crawl;
pub mod engine;
pub mod error;
pub mod net;
pub mod plan;
pub mod proxy;

pub use engine::{Mode, Options, Report, download};
pub use error::{Error, Result};
