import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";

// Coalesce bursts and serialize refreshes; events during a load request one
// more snapshot instead of racing a stale response.
export function useStoreChanged(onChange: () => void | Promise<void>, kinds?: string[]) {
  const callback = useRef(onChange);
  callback.current = onChange;
  const filter = kinds?.join(",");
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let running = false;
    let pending = false;
    const refresh = async () => {
      if (disposed) return;
      if (running) { pending = true; return; }
      running = true;
      try { await callback.current(); }
      finally {
        running = false;
        if (pending && !disposed) { pending = false; timer = setTimeout(() => void refresh(), 200); }
      }
    };
    void listen<string>("store-changed", ({ payload }) => {
      if (filter && !filter.split(",").includes(payload)) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => void refresh(), 200);
    }).then(fn => { if (disposed) fn(); else unlisten = fn; });
    return () => { disposed = true; if (timer) clearTimeout(timer); unlisten?.(); };
  }, [filter]);
}
