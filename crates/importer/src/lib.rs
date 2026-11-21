//! Импортёр JSON-источника: устойчивое чтение файла ([`reader`]), транзакция импорта
//! ([`service`]) и цикл опроса с advisory lock `0x4D545301`, общим с Python-версией ([`worker`]).

pub mod reader;
pub mod service;
pub mod worker;

pub use service::{ImportError, ImportOutcome, ImportService, OutcomeStatus};
pub use worker::{IMPORTER_LOCK_KEY, ImporterWorker, run_importer};
