//! Безопасное чтение полного JSON-снимка с диска — порт `app/importer/reader.py`.
//!
//! * Файл не перечитывается, если не изменились размер, mtime и inode ([`ReadResult::Unchanged`]).
//! * Устойчивость: stat до и после чтения должны совпасть, а размер прочитанного — с размером
//!   файла. Если файл меняется во время чтения (редактор сохраняет «на месте»), чтение повторяется.
//! * Атомарная публикация (запись во временный файл + rename) читается всегда целиком.
//!
//! Функция синхронная; из tokio вызывается через `spawn_blocking`.

use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const DEFAULT_MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileFingerprint {
    pub mtime_ns: i64,
    pub size: u64,
    pub inode: u64,
}

impl FileFingerprint {
    pub fn from_metadata(meta: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                size: meta.size(),
                inode: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
            Self {
                mtime_ns,
                size: meta.len(),
                inode: 0,
            }
        }
    }
}

/// Вид отказа — те же значения, что `FailureKind` Python-версии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Missing,
    Unstable,
    IoError,
    TooLarge,
    NotAFile,
}

impl FailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unstable => "unstable",
            Self::IoError => "io_error",
            Self::TooLarge => "too_large",
            Self::NotAFile => "not_a_file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadResult {
    Ok {
        content: Vec<u8>,
        sha256: String,
        fingerprint: FileFingerprint,
    },
    Unchanged {
        fingerprint: FileFingerprint,
    },
    Failed {
        kind: FailureKind,
        message: String,
    },
}

impl ReadResult {
    fn failed(kind: FailureKind, message: String) -> Self {
        Self::Failed { kind, message }
    }

    /// Отпечаток прочитанного (или не изменившегося) файла.
    pub fn fingerprint(&self) -> Option<FileFingerprint> {
        match self {
            Self::Ok { fingerprint, .. } | Self::Unchanged { fingerprint } => Some(*fingerprint),
            Self::Failed { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOptions {
    pub attempts: u32,
    pub retry_delay: Duration,
    pub max_bytes: u64,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            retry_delay: Duration::from_millis(50),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Имя файла для сообщений (`path.name`).
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `exc.strerror or exc`: текст ОС без хвоста « (os error N)», который добавляет Rust.
fn strerror(e: &io::Error) -> String {
    let text = e.to_string();
    match (e.raw_os_error(), text.rfind(" (os error ")) {
        (Some(_), Some(pos)) => text[..pos].to_owned(),
        _ => text,
    }
}

/// Читает файл целиком в устойчивом состоянии.
///
/// `known` — отпечаток последнего успешно обработанного содержимого: если файл не менялся,
/// возвращается `Unchanged` без чтения.
pub fn read_snapshot_file(path: &Path, known: Option<FileFingerprint>, options: &ReadOptions) -> ReadResult {
    read_snapshot_file_with(path, known, options, &mut std::thread::sleep, &mut |_| {})
}

/// То же с внедряемыми `sleep` и `after_open` (вызывается сразу после открытия файла) — для
/// тестов, которые в Python подменяют `time.sleep` и `Path.open`.
pub fn read_snapshot_file_with(
    path: &Path,
    known: Option<FileFingerprint>,
    options: &ReadOptions,
    sleep: &mut dyn FnMut(Duration),
    after_open: &mut dyn FnMut(&Path),
) -> ReadResult {
    for attempt in 0..options.attempts {
        let before = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return ReadResult::failed(
                    FailureKind::Missing,
                    format!("Файл источника не найден: {}", file_name(path)),
                );
            }
            Err(e) => {
                return ReadResult::failed(
                    FailureKind::IoError,
                    format!("Не удалось получить сведения о файле: {}", strerror(&e)),
                );
            }
        };
        if !before.is_file() {
            return ReadResult::failed(
                FailureKind::NotAFile,
                format!("Источник не является файлом: {}", file_name(path)),
            );
        }
        let fingerprint = FileFingerprint::from_metadata(&before);
        if known == Some(fingerprint) {
            return ReadResult::Unchanged { fingerprint };
        }
        if before.len() > options.max_bytes {
            return ReadResult::failed(
                FailureKind::TooLarge,
                format!("Файл источника больше {} МБ", options.max_bytes / (1024 * 1024)),
            );
        }

        let read = File::open(path).and_then(|file| {
            after_open(path);
            let mut content = Vec::new();
            file.take(options.max_bytes + 1).read_to_end(&mut content)?;
            let after = fs::metadata(path)?;
            Ok((content, after))
        });
        let stable = match read {
            Ok((content, after)) => {
                let same = FileFingerprint::from_metadata(&after) == fingerprint
                    && content.len() as u64 == after.len();
                same.then_some(content)
            }
            // Файл заменили между stat и open (атомарная публикация) — повторяем.
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return ReadResult::failed(
                    FailureKind::IoError,
                    format!("Ошибка чтения файла источника: {}", strerror(&e)),
                );
            }
        };
        if let Some(content) = stable {
            let sha256 = hex::encode(Sha256::digest(&content));
            return ReadResult::Ok {
                content,
                sha256,
                fingerprint,
            };
        }
        if attempt + 1 < options.attempts {
            sleep(options.retry_delay);
        }
    }
    ReadResult::failed(
        FailureKind::Unstable,
        "Файл источника изменяется во время чтения".to_owned(),
    )
}
