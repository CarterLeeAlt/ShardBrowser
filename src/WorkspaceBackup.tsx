import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, Upload } from "./assets/icons/Lucide";
import { tr } from "./i18n";

const FOLDERS_KEY = "shardx-folders";
const FOLDERS_CHANGED = "shardx-folders-changed";
const EXTENSION = "shardx-workspace";

function readWorkspaceFolders(): string[] {
  try {
    const folders: unknown = JSON.parse(localStorage.getItem(FOLDERS_KEY) ?? "[]");
    if (Array.isArray(folders) && folders.every((name): name is string => typeof name === "string")) return [...new Set(folders)];
  } catch { /* A damaged registry cannot supply folder names. */ }
  return [];
}

function writeWorkspaceFolders(folders: string[]) {
  localStorage.setItem(FOLDERS_KEY, JSON.stringify(folders));
  window.dispatchEvent(new Event(FOLDERS_CHANGED));
}

export function updateWorkspaceFolders(update: (folders: string[]) => string[]) {
  writeWorkspaceFolders(update(readWorkspaceFolders()));
}

export function useWorkspaceFolders() {
  const [folders, setFolders] = useState(readWorkspaceFolders);
  useEffect(() => {
    const refresh = () => setFolders(readWorkspaceFolders());
    window.addEventListener(FOLDERS_CHANGED, refresh);
    refresh();
    return () => window.removeEventListener(FOLDERS_CHANGED, refresh);
  }, []);
  return folders;
}

type Summary = { id: string; profiles: number; proxies: number; folders: string[] };

export function useWorkspaceBackup(confirm: (title: string, message: string) => Promise<boolean>) {
  const running = useRef(false);
  const [status, setStatus] = useState<"export" | "inspect" | "confirm" | "restore" | null>(null);
  const [result, setResult] = useState<{ error: boolean; message: string } | null>(null);
  const filter = [{ name: tr("ShardX workspace backup"), extensions: [EXTENSION] }];

  const run = async (mode: "export" | "restore") => {
    if (running.current) return;
    running.current = true;
    setStatus(mode === "export" ? "export" : "inspect");
    setResult(null);
    try {
      if (mode === "export") {
        const stamp = new Date().toISOString().replace(/[:.]/g, "-");
        const path = await save({ title: tr("Export complete backup"), defaultPath: `ShardX-${stamp}.${EXTENSION}`, filters: filter });
        if (!path) return;
        const summary = await invoke<Summary>("workspace_backup_export", { path, folders: readWorkspaceFolders() });
        setResult({ error: false, message: tr("Backed up {0} browser profiles and {1} proxies to {2}.", summary.profiles, summary.proxies, path) });
      } else {
        const path = await open({ title: tr("Restore from backup"), multiple: false, filters: filter });
        if (typeof path !== "string") return;
        if (readWorkspaceFolders().length) throw new Error(tr("Restore requires an empty workspace (custom folders already exist)"));
        const summary = await invoke<Summary>("workspace_backup_inspect", { path });
        setStatus("confirm");
        if (!await confirm(tr("Restore from backup"), tr("Restore {0} browser profiles and {1} proxies into this empty workspace?", summary.profiles, summary.proxies))) return;
        if (readWorkspaceFolders().length) throw new Error(tr("Restore requires an empty workspace (custom folders already exist)"));
        setStatus("restore");
        const restored = await invoke<Summary>("workspace_backup_restore", { path, id: summary.id });
        try {
          writeWorkspaceFolders(restored.folders);
        } catch {
          throw new Error(tr("Browser profiles and proxies were restored, but custom folders could not be saved."));
        }
        setResult({ error: false, message: tr("Restored {0} browser profiles and {1} proxies.", restored.profiles, restored.proxies) });
      }
    } catch (error) {
      setResult({ error: true, message: tr(error instanceof Error ? error.message : String(error)) });
    } finally {
      running.current = false;
      setStatus(null);
    }
  };

  return { run, status, result };
}

export function WorkspaceBackup({ run, status, result }: ReturnType<typeof useWorkspaceBackup>) {
  return <section className="card settings-card" aria-labelledby="workspace-backup-title">
    <h3 id="workspace-backup-title">{tr("Backup and restore")}</h3>
    <div className="row-inline">
      <button className="btn-primary" disabled={status !== null} onClick={() => void run("export")}><Download /> {tr("Export complete backup")}</button>
      <button className="btn-ghost" disabled={status !== null} onClick={() => void run("restore")}><Upload /> {tr("Restore from backup")}</button>
    </div>
    {status && status !== "confirm" && <p role="status" className="muted">{tr(status === "export" ? "Exporting backup…" : status === "inspect" ? "Verifying backup…" : "Restoring backup…")}</p>}
    {result && <p role={result.error ? "alert" : "status"} className={result.error ? "storage-error" : "muted"}>{result.message}</p>}
  </section>;
}
