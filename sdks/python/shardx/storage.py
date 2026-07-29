"""Crash-safe helpers for SDK JSON and profile storage."""
from __future__ import annotations

import json
import os
import shutil
import uuid
from pathlib import Path
from typing import Any


def _publish(path: Path, data: str | bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.parent / f".{path.name}.{uuid.uuid4().hex}.tmp"
    mode = "wb" if isinstance(data, bytes) else "w"
    kwargs = {} if isinstance(data, bytes) else {"encoding": "utf-8", "newline": "\n"}
    try:
        with tmp.open(mode, **kwargs) as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(tmp, path)
    except Exception:
        tmp.unlink(missing_ok=True)
        raise


def atomic_write(path: Path, data: str | bytes) -> None:
    path = Path(path)
    if path.exists():
        backup = path.with_name(f"{path.name}.bak")
        backup_tmp = backup.with_name(f".{backup.name}.{uuid.uuid4().hex}.tmp")
        try:
            shutil.copyfile(path, backup_tmp)
            with backup_tmp.open("rb+") as output:
                os.fsync(output.fileno())
            os.replace(backup_tmp, backup)
        except Exception:
            backup_tmp.unlink(missing_ok=True)
            raise
    _publish(path, data)


def read_json_with_backup(path: Path) -> Any:
    path = Path(path)
    primary = path.read_text(encoding="utf-8")
    try:
        return json.loads(primary)
    except json.JSONDecodeError as primary_error:
        backup = path.with_name(f"{path.name}.bak")
        backup_text = backup.read_text(encoding="utf-8")
        value = json.loads(backup_text)
        _publish(path, backup_text)
        print(
            f"[shardx] restored corrupted JSON {path} from {backup}: {primary_error}",
            flush=True,
        )
        return value
