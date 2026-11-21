#!/usr/bin/env python3
"""Сценарий демонстрации (ТЗ §19, ARCHITECTURE.md): изменения копии выгрузки по шагам.

Каждое изменение публикуется атомарно (временный файл + rename), как рекомендует ARCHITECTURE.md,
поэтому импортёр всегда читает целый снимок. Скрипт без зависимостей: запускается на хосте.

    python tools/demo.py status
    python tools/demo.py step1      # приоритет 3 → 4 + группа координаторов
    python tools/demo.py step2      # новый дедлайн (через 2 ч)
    python tools/demo.py step3      # Research / 20 + completionDate
    python tools/demo.py step4      # переназначение на инженеров
    python tools/demo.py step5      # повторное сохранение того же файла
    python tools/demo.py broken     # невалидный JSON
    python tools/demo.py restore    # вернуть последний корректный снимок
    python tools/demo.py reset      # вернуть исходную выгрузку
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from datetime import UTC, datetime, timedelta
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_FILE = ROOT / "data" / "character_problems.json"
DEMO_ID = "INC000021938167"
COORDINATORS = "КЦ ИТ Координаторы Массовых Инцидентов"
ENGINEERS = "ОГЭ Приморье"


def iso(dt: datetime) -> str:
    return dt.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def publish(path: Path, content: str) -> None:
    """Атомарная публикация полного снимка."""
    fd, tmp = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(content)
            fh.flush()
            os.fsync(fh.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


class Demo:
    def __init__(self, path: Path, incident_id: str = DEMO_ID):
        self.path = path
        self.incident_id = incident_id
        self.state_dir = path.parent / ".demo"
        self.original = self.state_dir / "original.json"
        self.last_valid = self.state_dir / "last_valid.json"

    # ------------------------------------------------------------ файлы

    def _ensure_backup(self) -> None:
        self.state_dir.mkdir(exist_ok=True)
        if not self.original.exists():
            shutil.copyfile(self.path, self.original)

    def _load(self) -> list[dict]:
        return json.loads(self.path.read_text(encoding="utf-8"))

    def _save(self, records: list[dict]) -> None:
        content = json.dumps(records, ensure_ascii=False, indent=2) + "\n"
        publish(self.path, content)
        self.last_valid.write_text(content, encoding="utf-8")

    def _edit(self, **changes) -> dict:
        self._ensure_backup()
        records = self._load()
        record = next((r for r in records if r.get("id") == self.incident_id), None)
        if record is None:
            raise SystemExit(f"Инцидент {self.incident_id} не найден в {self.path}")
        record.update(changes)
        self._save(records)
        return record

    # ------------------------------------------------------------ шаги

    def step1(self) -> str:
        self._edit(priorityId=4, priority="Наивысший", assignedGroupName=COORDINATORS)
        return "priorityId 3 → 4, назначен на координаторов: «Критические» + «На координаторах», 2 оповещения"

    def step2(self) -> str:
        deadline = iso(datetime.now(UTC) + timedelta(hours=2))
        self._edit(deadline=deadline)
        return f"Новый дедлайн {deadline}: таймер пересчитан, новых оповещений нет"

    def step3(self) -> str:
        self._edit(
            stateEnum="Research",
            stateId=20,
            state="Исследование ситуации",
            completionDate=iso(datetime.now(UTC)),
        )
        return (
            "Research / 20 + completionDate: «Исследование» и «На координаторах», отметка «Решён», оповещение"
        )

    def step4(self) -> str:
        self._edit(assignedGroupName=ENGINEERS)
        return f"Переназначен на «{ENGINEERS}»: уходит из координаторов, остаётся в исследовании"

    def step5(self) -> str:
        self._ensure_backup()
        content = self.path.read_text(encoding="utf-8")
        publish(self.path, content)
        return "Тот же снимок сохранён повторно: ни изменений, ни дублей оповещений"

    def broken(self) -> str:
        self._ensure_backup()
        if not self.last_valid.exists():
            shutil.copyfile(self.path, self.last_valid)
        publish(self.path, '[{"id": "INC000021938167", "priorityId": ')
        return "Записан невалидный JSON: дашборд показывает последний принятый снимок и предупреждение"

    def restore(self) -> str:
        source = self.last_valid if self.last_valid.exists() else self.original
        if not source.exists():
            raise SystemExit("Нет сохранённого корректного снимка")
        publish(self.path, source.read_text(encoding="utf-8"))
        return "Корректный снимок восстановлен: обновления возобновляются автоматически"

    def reset(self) -> str:
        if not self.original.exists():
            return "Исходная выгрузка не изменялась"
        content = self.original.read_text(encoding="utf-8")
        publish(self.path, content)
        self.last_valid.write_text(content, encoding="utf-8")
        return "Возвращена исходная выгрузка"

    def status(self) -> str:
        try:
            records = self._load()
        except json.JSONDecodeError as exc:
            return f"Файл сейчас невалиден: {exc}"
        r = next((r for r in records if r.get("id") == self.incident_id), {})
        keys = ("stateEnum", "stateId", "priorityId", "assignedGroupName", "deadline", "completionDate")
        return f"{self.incident_id}: " + ", ".join(f"{k}={r.get(k)!r}" for k in keys)


STEPS = ("status", "step1", "step2", "step3", "step4", "step5", "broken", "restore", "reset")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Сценарий демонстрации дашборда")
    parser.add_argument("step", choices=STEPS)
    parser.add_argument("--file", type=Path, default=DEFAULT_FILE, help="Файл выгрузки (копия!)")
    parser.add_argument("--incident", default=DEMO_ID)
    args = parser.parse_args(argv)
    demo = Demo(args.file, args.incident)
    message = getattr(demo, args.step)()
    print(f"[{datetime.now().strftime('%H:%M:%S')}] {args.step}: {message}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
