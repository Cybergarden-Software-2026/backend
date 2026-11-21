//! Порт тестов `reader` из `backend/tests/unit/test_stage5_importer.py`.

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use dashboard_importer::reader::{
    FailureKind, ReadOptions, ReadResult, read_snapshot_file, read_snapshot_file_with,
};

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rust_reader_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn content(result: &ReadResult) -> &[u8] {
    match result {
        ReadResult::Ok { content, .. } => content,
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn reader_reads_full_file_and_detects_unchanged() {
    let dir = tmp_dir("unchanged");
    let path = dir.join("s.json");
    std::fs::write(&path, b"[]").unwrap();
    let opts = ReadOptions::default();
    let first = read_snapshot_file(&path, None, &opts);
    assert_eq!(content(&first), b"[]");
    assert!(matches!(
        read_snapshot_file(&path, first.fingerprint(), &opts),
        ReadResult::Unchanged { .. }
    ));

    std::fs::write(&path, b"[ ]").unwrap();
    let second = read_snapshot_file(&path, first.fingerprint(), &opts);
    let (ReadResult::Ok { sha256: a, .. }, ReadResult::Ok { sha256: b, .. }) = (&first, &second) else {
        panic!("expected Ok, got {second:?}");
    };
    assert_ne!(a, b);
    // sha256 — тот же hexdigest, что hashlib.
    assert_eq!(
        a,
        "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
    );
}

#[test]
fn reader_atomic_replace_is_detected() {
    let dir = tmp_dir("atomic");
    let path = dir.join("s.json");
    std::fs::write(&path, b"[1]").unwrap();
    let opts = ReadOptions::default();
    let first = read_snapshot_file(&path, None, &opts);
    let tmp = dir.join("s.json.tmp");
    std::fs::write(&tmp, b"[2]").unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    let result = read_snapshot_file(&path, first.fingerprint(), &opts);
    assert_eq!(content(&result), b"[2]");
}

#[test]
fn reader_missing_file() {
    let dir = tmp_dir("missing");
    let result = read_snapshot_file(&dir.join("nope.json"), None, &ReadOptions::default());
    assert_eq!(
        result,
        ReadResult::Failed {
            kind: FailureKind::Missing,
            message: "Файл источника не найден: nope.json".into()
        }
    );
}

#[test]
fn reader_not_a_file() {
    let dir = tmp_dir("dir");
    let result = read_snapshot_file(&dir, None, &ReadOptions::default());
    assert!(matches!(
        result,
        ReadResult::Failed {
            kind: FailureKind::NotAFile,
            ..
        }
    ));
}

#[test]
fn reader_too_large() {
    let dir = tmp_dir("large");
    let path = dir.join("s.json");
    std::fs::write(&path, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    let opts = ReadOptions {
        max_bytes: 2 * 1024 * 1024,
        ..ReadOptions::default()
    };
    assert_eq!(
        read_snapshot_file(&path, None, &opts),
        ReadResult::Failed {
            kind: FailureKind::TooLarge,
            message: "Файл источника больше 2 МБ".into()
        }
    );
}

/// Файл дописывается во время чтения: первое чтение неустойчиво, повтор — успешен.
#[test]
fn reader_retries_while_file_changes() {
    let dir = tmp_dir("retry");
    let path = dir.join("s.json");
    std::fs::write(&path, b"[").unwrap();
    let calls = Cell::new(0);
    let mut sleep = |_| {
        calls.set(calls.get() + 1);
        std::fs::write(&path, b"[1, 2, 3]").unwrap();
    };
    let mut after_open = |p: &Path| {
        if calls.get() == 0 {
            append(p, b"1"); // запись «на месте» во время чтения
        }
    };
    let result = read_snapshot_file_with(&path, None, &ReadOptions::default(), &mut sleep, &mut after_open);
    assert_eq!(calls.get(), 1);
    assert_eq!(content(&result), b"[1, 2, 3]");
}

#[test]
fn reader_gives_up_when_never_stable() {
    let dir = tmp_dir("unstable");
    let path = dir.join("s.json");
    std::fs::write(&path, b"[]").unwrap();
    let result = read_snapshot_file_with(&path, None, &ReadOptions::default(), &mut |_| {}, &mut |p| {
        append(p, b" ")
    });
    assert_eq!(
        result,
        ReadResult::Failed {
            kind: FailureKind::Unstable,
            message: "Файл источника изменяется во время чтения".into()
        }
    );
}
