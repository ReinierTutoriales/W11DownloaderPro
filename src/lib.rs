//! Motor de descargas de W11DownloaderPro.

pub mod crawl;
pub mod engine;
pub mod error;
pub mod limits;
pub mod merge;
pub mod naming;
pub mod native;
pub mod net;
pub mod plan;
pub mod proxy;
pub mod queue;
pub mod register;
pub mod runtime;
pub mod schedule;
pub mod video;

pub use engine::{Mode, Options, Report, download};
pub use error::{Error, Result};
