import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useStoreChanged } from "./useStoreChanged";
import { tr } from "./i18n";
import type { StorageViewProps } from "./Extensions";
import { Trash } from "./assets/icons/Lucide";
import { ListState, PageToolbar, SelectionBar } from "./Workspace";

type TrashEntry = {
  id: string; profile_id: string; name: string; folder: string;
  deleted_at: number; expires_at: number; size_bytes: number; extensions: string[];
};

export function TrashView({ confirm, onSuccess, onError }: StorageViewProps) {
  const [entries, setEntries] = useState<TrashEntry[]>([]);
  const [selected, setSelected] = useState(new Set<string>());
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const reload = async () => {
    try {
      const current = await invoke<TrashEntry[]>("trash_list");
      setEntries(current);
      setSelected((ids) => new Set([...ids].filter((id) => current.some((entry) => entry.id === id))));
      setError("");
    } catch (e) { setError(String(e)); }
    finally { setLoading(false); }
  };
  useEffect(() => { void reload(); }, []);
  useStoreChanged(reload, ["trash"]);
  const restore = async (ids: string[]) => {
    setBusy(true);
    let count = 0;
    try {
      for (const id of ids) {
        try { await invoke("trash_restore", { id }); count += 1; }
        catch (e) { onError(tr("Could not restore {0}: {1}", entries.find((entry) => entry.id === id)?.name ?? id, tr(String(e)))); }
      }
      if (count) onSuccess(tr("Restored {0} profiles", count));
    } finally { setBusy(false); }
  };
  const purge = async (all: boolean) => {
    const count = all ? entries.length : selected.size;
    if (!count || !await confirm(all ? tr("Empty recycle bin") : tr("Delete permanently"), tr("Permanently delete {0} profiles and their browser data? This cannot be undone.", count))) return;
    setBusy(true);
    try {
      const removed = await invoke<number>(all ? "trash_empty" : "trash_purge", all ? {} : { ids: [...selected] });
      onSuccess(tr("Permanently deleted {0} profiles", removed));
    } catch (e) { onError(tr(String(e))); }
    finally { setBusy(false); }
  };
  const date = (seconds: number) => new Date(seconds * 1000).toLocaleString("zh-CN");
  return <section className="page">
    <PageToolbar title={tr("Recycle bin")}>
      <button className="btn-ghost danger" disabled={busy || !entries.length} onClick={() => void purge(true)}><Trash /> {tr("Empty recycle bin")}</button>
    </PageToolbar>
    {error && <ListState kind="error" text={tr("Unable to load recycle bin: {0}", error)} />}
    <SelectionBar count={selected.size}>
      <button className="btn-ghost btn-sm" disabled={busy} onClick={() => void restore([...selected])}>{tr("Restore selected")}</button>
      <button className="btn-ghost btn-sm danger" disabled={busy} onClick={() => void purge(false)}>{tr("Delete permanently")}</button>
    </SelectionBar>
    {loading ? <ListState kind="loading" text={tr("Loading…")} /> : entries.length === 0 ? !error && <ListState kind="empty" text={tr("Recycle bin is empty")} /> :
      <div className="storage-table-wrap"><table className="storage-table">
        <thead><tr><th><input type="checkbox" aria-label={tr("Select all")} disabled={busy} checked={selected.size === entries.length} onChange={(e) => setSelected(new Set(e.target.checked ? entries.map((entry) => entry.id) : []))} /></th><th>{tr("Name")}</th><th>{tr("Folder")}</th><th>{tr("Deleted at")}</th><th>{tr("Expires at")}</th><th>{tr("Data size")}</th><th>{tr("Actions")}</th></tr></thead>
        <tbody>{entries.map((entry) => <tr key={entry.id}>
          <td><input type="checkbox" aria-label={entry.name} disabled={busy} checked={selected.has(entry.id)} onChange={(e) => setSelected((current) => { const next = new Set(current); if (e.target.checked) next.add(entry.id); else next.delete(entry.id); return next; })} /></td>
          <td>{entry.name}<div className="muted table-detail">{entry.profile_id}</div></td><td>{entry.folder || "—"}</td>
          <td>{date(entry.deleted_at)}</td><td>{date(entry.expires_at)}</td><td>{(entry.size_bytes / 1024 / 1024).toFixed(1)} MiB</td>
          <td><button className="btn-ghost btn-sm" disabled={busy || Date.now() >= entry.expires_at * 1000} onClick={() => void restore([entry.id])}>{tr("Restore")}</button></td>
        </tr>)}</tbody>
      </table></div>}
  </section>;
}
