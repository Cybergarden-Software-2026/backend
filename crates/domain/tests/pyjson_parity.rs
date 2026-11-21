//! Сообщения о синтаксических ошибках JSON совпадают с Python-версией (`parse_json`,
//! CPython 3.12): корпус `fixtures/pyjson_python.json` из `gen_pyjson.py` — обрывы файла в каждой
//! позиции (в том числе внутри кириллицы и эмодзи), случайные правки и крайние случаи
//! экранирования, чисел, констант, BOM и «Extra data».

use dashboard_domain::pyjson::locate_error;
use dashboard_domain::validator::parse_json;
use serde_json::Value;

fn apply(spec: &Value, bases: &[Vec<char>]) -> String {
    if let Some(text) = spec.get("text") {
        return text.as_str().unwrap().to_owned();
    }
    let base = &bases[spec["base"].as_u64().unwrap() as usize];
    if let Some(cut) = spec.get("cut") {
        return base[..cut.as_u64().unwrap() as usize].iter().collect();
    }
    let edit = spec["edit"].as_array().unwrap();
    let pos = edit[0].as_u64().unwrap() as usize;
    let delete = edit[1].as_u64().unwrap() as usize;
    let insert = edit[2].as_str().unwrap();
    let mut out: String = base[..pos].iter().collect();
    out.push_str(insert);
    out.extend(base[(pos + delete).min(base.len())..].iter());
    out
}

#[test]
fn json_errors_match_cpython() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pyjson_python.json")).unwrap();
    assert_eq!(&fixture["python"].as_str().unwrap()[..5], "3.12.");
    let bases: Vec<Vec<char>> = fixture["bases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().chars().collect())
        .collect();
    let (mut rejected, mut accepted_by_python_only) = (0, 0);
    let mut mismatches = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let text = apply(case, &bases);
        let rust = parse_json(text.as_bytes()).err().map(|e| e.summary());
        match case["python"].as_str() {
            Some(python) => {
                rejected += 1;
                if rust.as_deref() != Some(python) {
                    mismatches.push(format!("{text:.80?}\n  python: {python}\n  rust:   {rust:?}"));
                }
            }
            // CPython принял текст. serde_json строже (одиночный суррогат, 1e400, глубина > 128) —
            // тогда локатор обязан согласиться, что CPython ошибки не нашёл бы.
            None => {
                if rust.is_some() {
                    accepted_by_python_only += 1;
                    assert_eq!(locate_error(&text), None, "{text:.80?}: {rust:?}");
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {rejected} differ:\n{}",
        mismatches.len(),
        mismatches[..mismatches.len().min(15)].join("\n")
    );
    assert!(rejected > 8000, "{rejected}");
    eprintln!("{rejected} rejected texts match; {accepted_by_python_only} accepted only by CPython");
}
