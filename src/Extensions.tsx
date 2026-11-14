import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useStoreChanged } from "./useStoreChanged";
import { open } from "@tauri-apps/plugin-dialog";
import { tr } from "./i18n";

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
  return <section className="page workspace-page">
    <div className="topbar"><div className="crumbs">{tr("Library")} › {tr("Extensions")}</div></div>
    <div className="page-title">
      <h1>{tr("Extension Library")}</h1>
      <div className="page-actions storage-actions">
        <button className="btn-ghost" disabled={busy} onClick={() => void importLocal(true)}>{tr("Import folder")}</button>
        <button className="btn-primary" disabled={busy} onClick={() => void importLocal(false)}>{busy ? tr("Importing…") : tr("Import CRX / ZIP")}</button>
      </div>
    </div>
    <p className="muted small">{tr("Import once, select per profile. Extension settings and data stay separate for each browser.")}</p>
    {error && <p className="storage-error" role="alert">{tr("Unable to load extensions: {0}", error)}</p>}
    {loading ? <p className="muted">{tr("Loading…")}</p> : entries.length === 0 ? <div className="empty-rich">{tr("No extensions yet")}</div> :
      <div className="extension-grid">{entries.map((entry) => {
        const referenced = entry.profile_names.length > 0 || entry.trash_count > 0 || !!entry.usage_error;
        return <article className="card extension-card" key={entry.id}>
          <header><h3>{entry.name}</h3><span className="lib-tag">{entry.version}</span></header>
          <p className="muted small">{entry.description}</p>
          <code className="extension-id">{entry.id}</code>
          {entry.error && <p className="storage-error" role="alert">{tr("Extension unavailable: {0}", entry.error)}</p>}
          {entry.usage_error && <p className="storage-error" role="alert">{entry.usage_error}</p>}
          <p className="small" title={entry.profile_names.join(", ")}>{tr("{0} profiles · {1} recycle-bin items", entry.profile_names.length, entry.trash_count)}</p>
          <button className="btn-ghost btn-sm danger" disabled={busy || referenced} title={referenced ? tr("Remove profile and recycle-bin references before deleting this extension") : tr("Delete extension")} onClick={() => void remove(entry)}>{tr("Delete")}</button>
        </article>;
      })}</div>}
  </section>;
}
