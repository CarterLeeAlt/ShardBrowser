import type { ReactNode } from "react";
import { Search } from "./assets/icons/Lucide";
import { tr } from "./i18n";

export function PageToolbar({ title, search, children }: {
  title: string;
  search?: { value: string; onChange: (value: string) => void };
  children?: ReactNode;
}) {
  return <header className="page-toolbar">
    {search ? <>
      <h1 className="sr-only">{title}</h1>
      <label className="page-search">
        <Search size={16} />
        <input type="search" aria-label={tr("Search")} placeholder={tr("Search…")} value={search.value}
          onChange={(event) => search.onChange(event.target.value)} />
      </label>
    </> : <h1>{title}</h1>}
    {children && <div className="page-actions">{children}</div>}
  </header>;
}

export function SelectionBar({ count, children }: { count: number; children: ReactNode }) {
  return count > 0 && <div className="bulk-bar">
    <span role="status">{tr("{0} selected", count)}</span>
    {children}
  </div>;
}

export function ListState({ kind, text }: {
  kind: "loading" | "empty" | "error";
  text: string;
}) {
  return <div className={`list-state list-state-${kind}`} role={kind === "error" ? "alert" : "status"}>
    {text}
  </div>;
}
