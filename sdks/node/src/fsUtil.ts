import {
  closeSync,
  copyFileSync,
  existsSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join } from "node:path";
import { randomUUID } from "node:crypto";

function publishFileSync(path: string, data: string | Buffer): void {
  const parent = dirname(path);
  mkdirSync(parent, { recursive: true });
  const tmp = join(parent, `.${basename(path)}.${randomUUID()}.tmp`);
  let fd: number | undefined;
  try {
    fd = openSync(tmp, "wx");
    writeFileSync(fd, data);
    fsyncSync(fd);
    closeSync(fd);
    fd = undefined;
    renameSync(tmp, path);
  } catch (error) {
    if (fd !== undefined) {
      try { closeSync(fd); } catch { /* best-effort cleanup */ }
    }
    rmSync(tmp, { force: true });
    throw error;
  }
}

export function atomicWriteFileSync(path: string, data: string | Buffer): void {
  if (existsSync(path)) {
    const backup = `${path}.bak`;
    const backupTmp = `${backup}.${randomUUID()}.tmp`;
    try {
      copyFileSync(path, backupTmp);
      const fd = openSync(backupTmp, "r+");
      try { fsyncSync(fd); } finally { closeSync(fd); }
      renameSync(backupTmp, backup);
    } catch (error) {
      rmSync(backupTmp, { force: true });
      throw error;
    }
  }
  publishFileSync(path, data);
}

export function readJsonWithBackupSync<T>(path: string): T {
  const primary = readFileSync(path, "utf8");
  try {
    return JSON.parse(primary) as T;
  } catch (primaryError) {
    const backup = `${path}.bak`;
    const backupText = readFileSync(backup, "utf8");
    const value = JSON.parse(backupText) as T;
    publishFileSync(path, backupText);
    console.error(`[shardx] restored corrupted JSON ${path} from ${backup}: ${String(primaryError)}`);
    return value;
  }
}
