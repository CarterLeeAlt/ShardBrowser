import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useStoreChanged } from "./useStoreChanged";
import { open } from "@tauri-apps/plugin-dialog";
import { tr } from "./i18n";
import { FileUp, FolderOpen, Trash } from "./assets/icons/Lucide";
import { ListState, PageToolbar } from "./Workspace";

export type ExtensionEntry = {
  id: string; name: string; version: string; description: string;
  profile_names: string[]; trash_count: number;
  error: string | null; usage_error: string | null;
};

function useExtensions() {
  const [entries, setEntries] = useState<ExtensionEntry[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const reload = async () => {
    try { setEntries(await invoke<ExtensionEntry[]>("extension_list")); setError(""); }
    catch (e) { setError(String(e)); }
    finally { setLoading(false); }
  };
  useEffect(() => { void reload(); }, []);
  useStoreChanged(reload, ["extensions", "profiles", "trash"]);
  return { entries, error, loading, reload };
}

export function ExtensionPicker({ value, onChange }: { value: string[]; onChange: (ids: string[]) => void }) {
  const { entries, error, loading } = useExtensions();
  const missing = value.filter((id) => !entries.some((entry) => entry.id === id));
  return (
    <div className="extension-picker">
      <div className="ie-section-title">{tr("Extensions")}</div>
      {loading ? <p className="muted small">{tr("Loading…")}</p> : <>
        {entries.length === 0 && <p className="muted small">{tr("Import extensions in the Extension Library, then select them here.")}</p>}
        {entries.map((entry) => <label className="extension-choice" key={entry.id}>
          <input type="checkbox" checked={value.includes(entry.id)} disabled={!!entry.error && !value.includes(entry.id)} onChange={(e) => onChange(e.target.checked ? [...value, entry.id] : value.filter((id) => id !== entry.id))} />
          <span>{entry.name} <span className="muted small">{entry.version}</span>{entry.error && <span className="storage-error"> — {entry.error}</span>}</span>
        </label>)}
        {missing.map((id) => <label className="extension-choice" key={id}>
          <input type="checkbox" checked onChange={() => onChange(value.filter((item) => item !== id))} />
          <span className="storage-error">{tr("Missing extension: {0}", id)}</span>
        </label>)}
      </>}
      {error && <p className="storage-error" role="alert">{tr("Unable to load extensions: {0}", error)}</p>}
    </div>
  );
}

export type StorageViewProps = {
  confirm: (title: string, message: string) => Promise<boolean>;
  onSuccess: (message: string) => void;
  onError: (message: string) => void;
};

export function ExtensionsView({ confirm, onSuccess, onError }: StorageViewProps) {
  const { entries, error, loading } = useExtensions();
  const [busy, setBusy] = useState(false);
  const importLocal = async (directory: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      const chosen = await open(directory ? { directory: true, multiple: false } : {
        multiple: true, filters: [{ name: tr("Extension packages"), extensions: ["crx", "zip"] }],
      });
      if (!chosen) return;
      let count = 0;
      for (const path of Array.isArray(chosen) ? chosen : [chosen]) {
        try { await invoke("extension_import", { path }); count += 1; }
        catch (e) { onError(tr("Could not import {0}: {1}", path, tr(String(e)))); }
      }
      if (count) onSuccess(tr("Imported {0} extensions", count));
    } catch (e) { onError(String(e)); }
    finally { setBusy(false); }
  };
  const remove = async (entry: ExtensionEntry) => {
    if (!await confirm(tr("Delete extension"), tr("Delete {0} from the extension library?", entry.name))) return;
    setBusy(true);
    try { await invoke("extension_delete", { id: entry.id }); onSuccess(tr("Extension deleted")); }
    catch (e) { onError(tr(String(e))); }
    finally { setBusy(false); }
  };
  return <section className="page">
    <PageToolbar title={tr("Extension Library")}>
      <button className="btn-ghost" disabled={busy} onClick={() => void importLocal(true)}><FolderOpen /> {tr("Import folder")}</button>
      <button className="btn-primary" disabled={busy} onClick={() => void importLocal(false)}><FileUp /> {busy ? tr("Importing…") : tr("Import CRX / ZIP")}</button>
    </PageToolbar>
    {error && <ListState kind="error" text={tr("Unable to load extensions: {0}", error)} />}
    {loading ? <ListState kind="loading" text={tr("Loading…")} /> : entries.length === 0 ? !error && <ListState kind="empty" text={tr("No extensions yet")} /> :
      <div className="library-grid">{entries.map((entry) => {
        const referenced = entry.profile_names.length > 0 || entry.trash_count > 0 || !!entry.usage_error;
        return <article className="library-card" key={entry.id}>
          <header className="library-card-head"><h3 title={entry.name}>{entry.name}</h3><span className="library-tag" title={entry.version}>{entry.version}</span></header>
          <p className="muted">{entry.description}</p>
          <code className="extension-id">{entry.id}</code>
          {entry.error && <p className="storage-error" role="alert">{tr("Extension unavailable: {0}", entry.error)}</p>}
          {entry.usage_error && <p className="storage-error" role="alert">{entry.usage_error}</p>}
          <footer className="library-card-foot">
            <span className="small" title={entry.profile_names.join(", ")}>{tr("{0} profiles · {1} recycle-bin items", entry.profile_names.length, entry.trash_count)}</span>
            <button className="icon-btn danger" aria-label={tr("Delete extension")} disabled={busy || referenced} title={referenced ? tr("Remove profile and recycle-bin references before deleting this extension") : tr("Delete extension")} onClick={() => void remove(entry)}><Trash /></button>
          </footer>
        </article>;
      })}</div>}
  </section>;
}
