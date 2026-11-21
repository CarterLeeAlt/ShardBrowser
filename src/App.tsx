import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import {
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  pointerWithin,
  useDndContext,
  useDroppable,
  useSensor,
  useSensors,
  type DragEndEvent,
  type CollisionDetection,
  type KeyboardCoordinateGetter,
  type UniqueIdentifier,
} from "@dnd-kit/core";
import {
  SortableContext,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  AppWindow, Building2, CircleAlert, CircleCheck, Clock, Copy, CopyPlus, Download,
  EllipsisVertical, FileUp, FingerprintPattern, Folder, FolderOpen, Globe, GripVertical, Info,
  LoaderCircle, Maximize, Minus, Moon, Network, Pencil, Play, Plus, Puzzle, RefreshCw,
  Save, Settings as SettingsIcon, Square, Sun, Trash, Upload, X,
} from "./assets/icons/Lucide";
import flagCodes from "./assets/icons/flag-codes.json";
import { ListState, PageToolbar, SelectionBar } from "./Workspace";
import "./fonts.css";
import "./App.css";
import { planAssignment } from "./proxy-assignment";
import { useStoreChanged } from "./useStoreChanged";
import { mergeProfileEdits } from "./profileEdit";
import { tr } from "./i18n";
import { ExtensionsView, ExtensionPicker } from "./Extensions";
import { TrashView } from "./Trash";
import { updateWorkspaceFolders, useWorkspaceBackup, useWorkspaceFolders, WorkspaceBackup } from "./WorkspaceBackup";

// Host OS of the launcher window (never spoofed) — drives default OS tab + titlebar.
function detectHostOs(): "macOS" | "Windows" | "Linux" {
  const ua = navigator.userAgent;
  if (/Windows/i.test(ua)) return "Windows";
  if (/Macintosh|Mac OS X/i.test(ua)) return "macOS";
  if (/Linux|X11|CrOS/i.test(ua)) return "Linux";
  return "macOS";
}
const HOST_OS = detectHostOs();

// OS clipboard via Tauri plugin (webview navigator.clipboard throws).
const clip = {
  write: (text: string) => invoke("clipboard_write", { text }),
  read: () => invoke<string>("clipboard_read"),
};
const MAX_JSON_IMPORT_BYTES = 16 * 1024 * 1024;

/// Read a JSON file selected by the user without exposing an arbitrary path to
/// the Rust backend. The file contents stay in-memory until they are imported.
const pickJsonText = () => new Promise<string | null>((resolve, reject) => {
  const input = document.createElement("input");
  input.type = "file";
  input.accept = ".json,application/json";
  input.style.display = "none";
  input.addEventListener("change", async () => {
    const file = input.files?.[0];
    if (!file) {
      input.remove();
      resolve(null);
      return;
    }
    try {
      if (file.size > MAX_JSON_IMPORT_BYTES) {
        throw new Error(tr("JSON import files must not exceed 16 MiB"));
      }
      resolve(await file.text());
    } catch (e) {
      reject(e);
    } finally {
      input.remove();
    }
  }, { once: true });
  input.addEventListener("cancel", () => {
    input.remove();
    resolve(null);
  }, { once: true });
  document.body.appendChild(input);
  input.click();
});

type ProfileBackupSummary = { profileCount: number; fileCount: number; dataBytes: number };

/// Profile backups are binary ZIP containers and may be many gigabytes, so
/// pass native file paths directly to Rust instead of reading them into the
/// webview as text/base64.
const pickProfileBackupFiles = async (): Promise<string[] | null> => {
  const selected = await openDialog({
    multiple: true,
    directory: false,
    filters: [{ name: tr("ShardX profile backup"), extensions: ["shardx-backup"] }],
  });
  if (!selected) return null;
  return Array.isArray(selected) ? selected : [selected];
};

// Single UTM tag appended to every outbound proxyshard.com link.
const UTM_QS = "utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher";
const withUtm = (url: string) => url + (url.includes("?") ? "&" : "?") + UTM_QS;
// Docs URL behind the proxy UDP/No-UDP pill.
const UDP_DOCS_URL = withUtm("https://docs.proxyshard.com/eng/our-products/about-udp");

// ---- toasts (global queue, auto-expiry; push via toast.ok / toast.err) ----

type ToastItem = { id: number; kind: "ok" | "err" | "info" | "warn"; text: string; closing: boolean };
const MAX_VISIBLE_TOASTS = 5;
const TOAST_DURATION_MS = 3000;
const TOAST_EXIT_MS = 140;
let toastSeq = 0;
const toastSubs = new Set<(items: ToastItem[]) => void>();
let toastList: ToastItem[] = [];
let pendingToastList: ToastItem[] = [];
let toastOverflowTransition = false;
const publishToasts = () => toastSubs.forEach((cb) => cb(toastList));

const closeToast = (id: number, onRemoved?: () => void) => {
  const target = toastList.find((item) => item.id === id);
  if (!target || target.closing) return;
  toastList = toastList.map((item) => item.id === id ? { ...item, closing: true } : item);
  publishToasts();
  setTimeout(() => {
    const next = toastList.filter((item) => item.id !== id);
    if (next.length === toastList.length) return;
    toastList = next;
    publishToasts();
    onRemoved?.();
    pumpPendingToasts();
  }, TOAST_EXIT_MS);
};

const mountToast = (item: ToastItem) => {
  toastList = [...toastList, item];
  publishToasts();
  setTimeout(() => closeToast(item.id), TOAST_DURATION_MS - TOAST_EXIT_MS);
};

function pumpPendingToasts() {
  if (toastOverflowTransition) return;

  while (pendingToastList.length > 0 && toastList.length < MAX_VISIBLE_TOASTS) {
    const next = pendingToastList.shift();
    if (!next) break;
    mountToast(next);
  }

  if (pendingToastList.length === 0 || toastList.length < MAX_VISIBLE_TOASTS) return;
  // A naturally expiring toast will create the next slot; do not start a
  // competing removal animation while any item is already leaving.
  if (toastList.some((item) => item.closing)) return;

  toastOverflowTransition = true;
  closeToast(toastList[0].id, () => {
    toastOverflowTransition = false;
    pumpPendingToasts();
  });
}

const pushToast = (kind: ToastItem["kind"], text: string) => {
  pendingToastList = [...pendingToastList, {
    id: ++toastSeq,
    kind,
    text,
    closing: false,
  }];
  pumpPendingToasts();
};
const toast = {
  ok: (t: string) => pushToast("ok", t),
  err: (t: string) => pushToast("err", tr(t)),
  info: (t: string) => pushToast("info", t),
  warn: (t: string) => pushToast("warn", t),
};

function ToastGlyph({ kind }: { kind: ToastItem["kind"] }) {
  const Glyph = kind === "ok" ? CircleCheck : kind === "info" ? Info : CircleAlert;
  return <Glyph />;
}

function ToastHost() {
  const [items, setItems] = useState<ToastItem[]>(toastList);
  useEffect(() => {
    toastSubs.add(setItems);
    return () => { toastSubs.delete(setItems); };
  }, []);
  if (items.length === 0) return null;
  return (
    <div className="toast-host">
      {items.map((t) => (
        <div
          key={t.id}
          className={`toast toast-${t.kind}${t.closing ? " toast-closing" : ""}`}
          role={t.kind === "err" ? "alert" : "status"}
          aria-atomic="true"
        >
          <span className="toast-icon"><ToastGlyph kind={t.kind} /></span>
          <span className="toast-message">{t.text}</span>
        </div>
      ))}
    </div>
  );
}

// ---- confirm modal (replaces unreliable native confirm) ----

type ConfirmButton = { label: string; value: any; danger?: boolean; primary?: boolean };
type ConfirmReq = {
  title?: string;
  message: string;
  buttons: ConfirmButton[];
  resolve: (v: any) => void;
};
let confirmSub: ((req: ConfirmReq | null) => void) | null = null;

function confirmModal(opts: {
  title?: string;
  message: string;
  buttons?: ConfirmButton[];
  danger?: boolean;
}): Promise<any> {
  return new Promise((resolve) => {
    const buttons =
      opts.buttons ?? [
        { label: tr("Cancel"), value: false },
        { label: opts.danger ? tr("Delete") : tr("OK"), value: true, danger: opts.danger, primary: !opts.danger },
      ];
    confirmSub?.({ title: opts.title, message: opts.message, buttons, resolve });
  });
}

function DialogBackdrop({
  children,
  onClose,
  dismissOnBackdrop = true,
}: {
  children: ReactNode;
  onClose: () => void;
  dismissOnBackdrop?: boolean;
}) {
  // Judge the press origin instead of the synthesized click target. A text
  // selection that starts inside a dialog and ends on the backdrop can retarget
  // its final click to the backdrop even though the user never clicked it.
  const handlePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (
      dismissOnBackdrop &&
      event.isPrimary &&
      event.button === 0 &&
      event.target === event.currentTarget
    ) {
      onClose();
    }
  };

  return (
    <div className="dialog-bg" onPointerDown={handlePointerDown}>
      {children}
    </div>
  );
}

function ConfirmHost() {
  const [req, setReq] = useState<ConfirmReq | null>(null);
  useEffect(() => {
    confirmSub = setReq;
    return () => { if (confirmSub === setReq) confirmSub = null; };
  }, []);
  if (!req) return null;
  const done = (v: any) => { req.resolve(v); setReq(null); };
  return (
    <DialogBackdrop onClose={() => done(null)}>
      <div className="dialog dialog-confirm">
        <header className="dialog-head">
          <h2>{req.title ?? tr("Confirm")}</h2>
          <button className="icon-btn" onClick={() => done(null)} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          <p className="confirm-msg">{req.message}</p>
        </div>
        <div className="confirm-actions">
          {req.buttons.map((b, i) => (
            <button
              key={i}
              className={`btn-sm ${b.primary ? "btn-primary" : "btn-ghost"} ${b.danger ? "danger" : ""}`}
              onClick={() => done(b.value)}
            >
              {b.label}
            </button>
          ))}
        </div>
      </div>
    </DialogBackdrop>
  );
}

// ---- context menu ----

type ContextItem = {
  label: string;
  onClick: () => void;
  danger?: boolean;
  sep?: boolean;
  disabled?: boolean;
  title?: string;
};
function useContextMenu() {
  const [menu, setMenu] = useState<{ x: number; y: number; items: ContextItem[] } | null>(null);
  const close = () => setMenu(null);
  useEffect(() => {
    if (!menu) return;
    const dismiss = () => close();
    window.addEventListener("click", dismiss);
    window.addEventListener("scroll", dismiss, true);
    return () => {
      window.removeEventListener("click", dismiss);
      window.removeEventListener("scroll", dismiss, true);
    };
  }, [menu]);
  const open = (e: React.MouseEvent, items: ContextItem[]) => {
    e.preventDefault();
    setMenu({ x: e.clientX, y: e.clientY, items });
  };
  // Clamp menu into viewport post-layout.
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!menu || !el) return;
    const { width, height } = el.getBoundingClientRect();
    const pad = 8;
    let left = menu.x;
    let top = menu.y;
    if (left + width > window.innerWidth - pad) {
      left = Math.max(pad, window.innerWidth - width - pad);
    }
    if (top + height > window.innerHeight - pad) {
      top = Math.max(pad, window.innerHeight - height - pad);
    }
    el.style.left = `${left}px`;
    el.style.top = `${top}px`;
  }, [menu]);
  const node = menu ? (
    <div ref={ref} className="ctx-menu" style={{ left: menu.x, top: menu.y }} onClick={(e) => e.stopPropagation()}>
      {menu.items.map((it, i) =>
        it.sep ? (
          <div key={i} className="ctx-sep" />
        ) : (
          <button
            key={i}
            className={`ctx-item ${it.danger ? "ctx-danger" : ""}`}
            onClick={() => { it.onClick(); close(); }}
            disabled={it.disabled}
            title={it.title}
          >
            {it.label}
          </button>
        ),
      )}
    </div>
  ) : null;
  return { open, node };
}

// ---- backend types ----

type ProfileMeta = {
  id: string;
  name: string;
  notes: string;
  proxy_id: string | null;
  last_launched_at: string | null;
  created_at: string | null;
  folder: string;
  /// Cumulative engine uptime in ms across every launch.  Increased when
  /// the engine exits — for the currently-running session add `running[id]`
  /// (Date.now() - sessionStartTs) on top.
  total_runtime_ms: number;
};
type ProxyEntry = {
  id: string;
  name: string;
  kind: "socks5" | "http" | "https";
  host: string;
  port: number;
  username?: string;
  password?: string;
  credentials_configured?: boolean;
  credentials_unavailable?: boolean;
  country: string;
  notes: string;
};
type ProxyCredentialsUpdate =
  | { mode: "keep" }
  | { mode: "replace"; username: string; password: string }
  | { mode: "clear" };
type ProxySaveRequest = {
  id: string;
  name: string;
  kind: ProxyEntry["kind"];
  host: string;
  port: number;
  country: string;
  notes: string;
  credentials: ProxyCredentialsUpdate;
};
type Settings = {
  theme: string;
  geo_checker?: string | null;
  screen_resolution_mode?: string | null;
  minimize_to_tray?: boolean;
  api_enabled?: boolean;
  api_port?: number;
};
type ApiInfo = {
  enabled: boolean;
  port: number;
  base_url: string;
  token: string;
  listener: { state: string; port: number | null; error: string | null };
};
type Section = "browsers" | "proxies" | "fingerprints" | "extensions" | "trash" | "settings";

/// Library fingerprint backing the editor GPU select; payload supplies the coherent base.
type FingerprintEntry = {
  id: string;
  label: string;
  platform: string;
  chrome: string;
  gpu: string;
  builtin: boolean;
  payload: any;
};

// ---- profile form ----

function displayNameError(name: string, original?: string, optional = false): string | null {
  if (name === original) return null;
  if (/[^A-Za-z0-9 ._()-]/.test(name)) {
    return tr("Use only English letters, digits, spaces, and . - _ ( )");
  }
  if (!optional && !name.trim()) return tr("Name is required");
  return null;
}

type NoiseMode = "real" | "auto";
type WebRtcMode = "auto" | "tcp_only" | "block";
type GeoMode = "auto" | "manual";

type ProfileForm = {
  extensions: string[];
  id: string;
  name: string;
  notes: string;
  proxy_id: string | null;

  gpu_preset_id: string;
  user_agent: string;
  hardware_concurrency: number;
  device_memory: number;
  /// Sec-CH-UA-Platform-Version override; empty = use donor preset's value.
  platform_version: string;

  timezone: string;
  language: string;

  webrtc: WebRtcMode;
  do_not_track: boolean;

  noise_canvas: NoiseMode;
  noise_webgl: NoiseMode;
  noise_audio: NoiseMode;
  noise_client_rects: NoiseMode;
  noise_sensors: NoiseMode;
  /// Fonts: "real" passes host fonts through; "auto" hides a ~3% per-profile subset.
  noise_fonts: NoiseMode;
  /// TCP ports the browser refuses to connect to (RDP/VNC/TeamViewer/Squid).
  blocked_ports: number[];

  geo_mode: GeoMode;
  geo_lat: number;
  geo_lng: number;
  geo_accuracy: number;

  media_audio_in: number;
  media_audio_out: number;
  media_video_in: number;
};

type HardwareConfig = {
  hardware_concurrency: number;
  device_memory: number;
};

type PresetEnrichPicks = HardwareConfig & {
  platform_version?: string;
  hardware_configs: HardwareConfig[];
};

const MEDIA_COUNT_OPTIONS = [0, 1, 2, 3];

/// Common remote-control/proxy ports to block from outgoing browser connects.
const DEFAULT_BLOCKED_PORTS = [
  3389, // RDP
  5900, // VNC
  5901, // VNC
  5800, // VNC HTTP
  7070, // RealVNC / RealAudio
  6568, // AnyDesk
  5938, // TeamViewer
  1080, // SOCKS
  8080, // HTTP proxy
  3128, // Squid
  3030, // misc
];

/// "auto" sentinel; the Rust launch resolver replaces with concrete TZ.
const AUTO_TZ = "auto";
const AUTO_LANG = "auto";

const TIMEZONES = [
  AUTO_TZ,
  "America/Chicago", "America/Denver", "America/Los_Angeles", "America/New_York",
  "America/Sao_Paulo", "America/Toronto",
  "Asia/Bangkok", "Asia/Dubai", "Asia/Hong_Kong", "Asia/Jakarta", "Asia/Kolkata",
  "Asia/Seoul", "Asia/Shanghai", "Asia/Singapore", "Asia/Tokyo",
  "Australia/Sydney",
  "Europe/Amsterdam", "Europe/Athens", "Europe/Berlin", "Europe/Bucharest",
  "Europe/Helsinki", "Europe/Istanbul", "Europe/Kyiv", "Europe/Lisbon",
  "Europe/London", "Europe/Madrid", "Europe/Moscow", "Europe/Paris",
  "Europe/Prague", "Europe/Rome", "Europe/Stockholm", "Europe/Warsaw",
  "Europe/Vienna", "Europe/Zurich",
  "Pacific/Auckland", "UTC",
];

const LOCALES: { code: string; label: string }[] = [
  { code: AUTO_LANG, label: "Auto (from proxy geo)" },
  { code: "en-US", label: "English (US)" },
  { code: "en-GB", label: "English (UK)" },
  { code: "en-CA", label: "English (Canada)" },
  { code: "en-AU", label: "English (Australia)" },
  { code: "de-DE", label: "Deutsch (Deutschland)" },
  { code: "es-ES", label: "Español (España)" },
  { code: "es-MX", label: "Español (México)" },
  { code: "fr-FR", label: "Français (France)" },
  { code: "it-IT", label: "Italiano" },
  { code: "nl-NL", label: "Nederlands" },
  { code: "pl-PL", label: "Polski" },
  { code: "pt-BR", label: "Português (Brasil)" },
  { code: "pt-PT", label: "Português (Portugal)" },
  { code: "ro-RO", label: "Română" },
  { code: "ru-RU", label: "Русский" },
  { code: "uk-UA", label: "Українська" },
  { code: "tr-TR", label: "Türkçe" },
  { code: "el-GR", label: "Ελληνικά" },
  { code: "cs-CZ", label: "Čeština" },
  { code: "sv-SE", label: "Svenska" },
  { code: "fi-FI", label: "Suomi" },
  { code: "no-NO", label: "Norsk" },
  { code: "da-DK", label: "Dansk" },
  { code: "hu-HU", label: "Magyar" },
  { code: "zh-CN", label: "中文 (简体)" },
  { code: "zh-TW", label: "中文 (繁體)" },
  { code: "ja-JP", label: "日本語" },
  { code: "ko-KR", label: "한국어" },
  { code: "ar-SA", label: "العربية" },
  { code: "he-IL", label: "עברית" },
  { code: "id-ID", label: "Bahasa Indonesia" },
  { code: "vi-VN", label: "Tiếng Việt" },
  { code: "th-TH", label: "ไทย" },
  { code: "hi-IN", label: "हिन्दी" },
];

/// Build accept-language chain (primary → base → English fallback).
function deriveAcceptLanguage(loc: string): string {
  if (!loc) return "en-US,en;q=0.9";
  const base = loc.split("-")[0];
  if (loc === "en-US") return "en-US,en;q=0.9";
  return `${loc},${base};q=0.9,en-US;q=0.8,en;q=0.7`;
}

function deriveLanguagesArray(loc: string): string[] {
  if (!loc) return ["en-US", "en"];
  const base = loc.split("-")[0];
  if (loc === "en-US") return ["en-US", "en"];
  return [loc, base, "en-US", "en"];
}

const defaultForm = (): ProfileForm => ({
  extensions: [],
  id: "",
  name: "",
  notes: "",
  proxy_id: null,

  // Empty until snapped to gpusForOs[0] by useEffect.
  gpu_preset_id: "",
  user_agent:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36",
  hardware_concurrency: 8,
  device_memory: 16,
  // Empty = inherit donor; setGpu refreshes via enrich_picks_for_preset.
  platform_version: "",

  timezone: AUTO_TZ,
  language: AUTO_LANG,

  webrtc: "block",
  do_not_track: false,

  noise_canvas: "auto",
  noise_webgl: "auto",
  noise_audio: "real",
  noise_client_rects: "real",
  noise_sensors: "real",
  noise_fonts: "real",
  blocked_ports: DEFAULT_BLOCKED_PORTS.slice(),

  geo_mode: "auto",
  geo_lat: 52.2297,
  geo_lng: 21.0122,
  geo_accuracy: 50,

  media_audio_in: 1,
  media_audio_out: 1,
  media_video_in: 1,
});

function fromStored(stored: any): ProfileForm {
  const f = defaultForm();
  if (!stored) return f;
  f.id = stored?._meta?.id ?? "";
  f.extensions = stored?._meta?.extensions ?? [];
  f.proxy_id = stored?._meta?.proxy_id ?? null;
  f.name = stored?.name ?? "";
  f.notes = stored?.notes ?? "";
  // Empty only when a legacy profile cannot be matched; existing identities
  // are never auto-replaced merely because the library link is unavailable.
  f.gpu_preset_id = stored?._meta?.gpu_preset_id ?? "";
  f.user_agent = stored?.navigator?.user_agent ?? f.user_agent;
  f.hardware_concurrency = stored?.navigator?.hardware_concurrency ?? 8;
  f.device_memory = stored?.navigator?.device_memory ?? 16;
  f.platform_version = stored?.navigator?.platform_version ?? stored?.client_hints?.platform_version ?? "";
  f.timezone = stored?.timezone ?? AUTO_TZ;
  f.language = stored?.navigator?.language ?? AUTO_LANG;
  f.webrtc = (stored?.webrtc === "replace" ? "tcp_only" : stored?.webrtc) ?? "block";
  f.do_not_track = stored?.navigator?.do_not_track === "1" || stored?.navigator?.do_not_track === true;

  const noise = stored?.noise ?? {};
  const noiseMode = (n: any): NoiseMode => (n?.enabled ? "auto" : "real");
  f.noise_canvas = noiseMode(noise.canvas);
  f.noise_webgl = noiseMode(noise.webgl);
  f.noise_audio = noiseMode(noise.audio);
  f.noise_client_rects = noiseMode(noise.client_rects);
  f.noise_sensors = noiseMode(noise.sensors);
  // Fonts default OFF (real); mirrors C++ default.
  f.noise_fonts = noiseMode(noise.fonts);
  f.blocked_ports = Array.isArray(stored?.blocked_ports)
    ? stored.blocked_ports.filter((n: any) => typeof n === "number")
    : DEFAULT_BLOCKED_PORTS.slice();

  const geo = stored?.geolocation ?? {};
  f.geo_mode = geo.mode === "manual" ? "manual" : "auto";
  f.geo_lat = typeof geo.latitude === "number" ? geo.latitude : f.geo_lat;
  f.geo_lng = typeof geo.longitude === "number" ? geo.longitude : f.geo_lng;
  f.geo_accuracy = typeof geo.accuracy === "number" ? geo.accuracy : f.geo_accuracy;

  const md = stored?.media_devices ?? {};
  f.media_audio_in = md.audio_input_count ?? 1;
  f.media_audio_out = md.audio_output_count ?? 1;
  f.media_video_in = md.video_input_count ?? 1;

  return f;
}

// "Maximally soft" anti-fingerprint defaults: the smallest perturbation that
// still shifts the fingerprint hash without visibly degrading rendering.
// max_offset can't drop below 1 (0 = off), so client_rects is already at its
// gentlest floor.
const WEBGL_NOISE_INTENSITY = 0.0005;
const CLIENT_RECTS_MAX_OFFSET = 1;

/// Build on-disk FingerprintConfig from the durable profile plus user-edited
/// fields. Existing profiles must keep their current screen/window and any
/// config fields the editor does not expose; rebuilding from the original
/// library template would undo the display clamp applied at creation time.
/// Switching the GPU preset is the one intentional full-template replacement.
function toStored(f: ProfileForm, lib: FingerprintEntry | null, existing: any | null = null, baseline: ProfileForm | null = null): any {
  const existingPresetId = existing?._meta?.gpu_preset_id ?? "";
  const source = f.id && existing && existingPresetId === f.gpu_preset_id
    ? existing
    : lib?.payload;
  const base: any = source ? JSON.parse(JSON.stringify(source)) : {};
  const changed = (...keys: (keyof ProfileForm)[]) => !baseline || source !== existing || keys.some(key => JSON.stringify(f[key]) !== JSON.stringify(baseline[key]));
  base._meta = { ...(existing?._meta ?? {}), id: f.id };
  if (changed("gpu_preset_id")) base._meta.gpu_preset_id = f.gpu_preset_id;
  if (existing?._revision) base._revision = existing._revision;
  if (changed("extensions")) base._meta.extensions = f.extensions;
  if (changed("proxy_id")) base._meta.proxy_id = f.proxy_id;
  if (changed("name")) base.name = f.name;
  if (changed("notes")) base.notes = f.notes;
  // "auto" sentinel: resolver replaces at launch; persists across edits.
  if (changed("timezone")) base.timezone = f.timezone;
  if (changed("webrtc")) base.webrtc = f.webrtc;
  base.navigator = { ...(base.navigator || {}) };
  if (changed("language")) {
    base.icu_locale = f.language === AUTO_LANG ? null : f.language;
    Object.assign(base.navigator, { language: f.language, accept_language: f.language === AUTO_LANG ? null : deriveAcceptLanguage(f.language), languages: f.language === AUTO_LANG ? null : deriveLanguagesArray(f.language) });
  }
  for (const key of ["user_agent", "hardware_concurrency", "device_memory"] as const) {
    if (changed(key)) base.navigator[key] = f[key];
  }
  if (changed("do_not_track")) base.navigator.do_not_track = f.do_not_track ? "1" : null;
  if (changed("platform_version") && f.platform_version) {
    base.navigator.platform_version = f.platform_version;
    base.client_hints = {
      ...(base.client_hints || {}),
      platform_version: f.platform_version,
    };
  }

  for (const [field, key] of [["media_audio_in", "audio_input_count"], ["media_audio_out", "audio_output_count"], ["media_video_in", "video_input_count"]] as const) {
    if (changed(field)) base.media_devices = { ...base.media_devices, [key]: f[field] };
  }
  if (changed("geo_mode", "geo_lat", "geo_lng", "geo_accuracy")) base.geolocation =
    f.geo_mode === "manual"
      ? { mode: "manual", latitude: f.geo_lat, longitude: f.geo_lng, accuracy: f.geo_accuracy }
      : { mode: "auto" };

  // seed: 0 is the "derive automatically" sentinel for new profiles. Existing
  // profiles retain every seed and active tuning value so an ordinary edit
  // cannot silently change their browser fingerprint.
  const priorNoise = existing?.noise ?? {};
  const noiseSeed = (slot: string): number => {
    if (!f.id) return 0;
    const seed = priorNoise?.[slot]?.seed;
    return typeof seed === "number" && Number.isInteger(seed) && seed > 0 ? seed : 0;
  };
  const activeNumber = (slot: string, key: string, fallback: number): number => {
    const value = priorNoise?.[slot]?.[key];
    return f.id && priorNoise?.[slot]?.enabled === true && typeof value === "number"
      ? value
      : fallback;
  };
  const editedNoise = {
    canvas:       { enabled: f.noise_canvas === "auto",       seed: noiseSeed("canvas") },
    webgl:        { enabled: f.noise_webgl === "auto",        seed: noiseSeed("webgl"), intensity: f.noise_webgl === "auto" ? activeNumber("webgl", "intensity", WEBGL_NOISE_INTENSITY) : 0 },
    audio:        { enabled: f.noise_audio === "auto",        seed: noiseSeed("audio") },
    client_rects: { enabled: f.noise_client_rects === "auto", seed: noiseSeed("client_rects"), max_offset: f.noise_client_rects === "auto" ? activeNumber("client_rects", "max_offset", CLIENT_RECTS_MAX_OFFSET) : 0 },
    sensors:      { enabled: f.noise_sensors === "auto",      seed: noiseSeed("sensors") },
    fonts:        { enabled: f.noise_fonts === "auto",        seed: noiseSeed("fonts") },
  };
  for (const slot of Object.keys(editedNoise) as (keyof typeof editedNoise)[]) {
    if (changed(`noise_${slot}` as keyof ProfileForm)) {
      base.noise = { ...base.noise, [slot]: { ...base.noise?.[slot], ...editedNoise[slot] } };
    }
  }
  if (changed("blocked_ports")) base.blocked_ports = [...f.blocked_ports].sort((a, b) => a - b);

  return base;
}

// ---- app shell ----

type Theme = "dark" | "light";

const TITLEBAR_HOVER_RESET_CLASS = "titlebar-hover-reset";

/**
 * Hiding a Tauri window preserves the WebView DOM. If the close button was
 * hovered when the window was hidden, Chromium can keep that stale :hover
 * state until it receives another pointer event. Blur the button immediately
 * and suppress that stale paint until the pointer really moves over the
 * restored window.
 */
function prepareTitlebarWindowHide(button: HTMLButtonElement) {
  button.blur();

  const root = document.documentElement;
  root.classList.add(TITLEBAR_HOVER_RESET_CLASS);
  const controller = new AbortController();
  const clearStaleHover = () => {
    root.classList.remove(TITLEBAR_HOVER_RESET_CLASS);
    controller.abort();
  };
  const options = { capture: true, once: true, signal: controller.signal };
  window.addEventListener("pointermove", clearStaleHover, options);
  window.addEventListener("pointerdown", clearStaleHover, options);
}

export default function App() {
  const storageViewProps = {
    confirm: async (title: string, message: string) => (await confirmModal({ title, message, danger: true })) === true,
    onSuccess: toast.ok,
    onError: toast.err,
  };
  const [section, setSection] = useState<Section>("browsers");
  const workspaceBackup = useWorkspaceBackup(async (title, message) => {
    setSection("settings");
    return (await confirmModal({ title, message, buttons: [
      { label: tr("Cancel"), value: false },
      { label: tr("Restore"), value: true, primary: true },
    ] })) === true;
  });
  useEffect(() => { document.querySelector(".main")?.scrollTo({ top: 0, left: 0 }); }, [section]);
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem("shardx-theme") as Theme) || "dark",
  );
  useEffect(() => {
    let disposed = false;
    let unlisten: undefined | (() => void);
    listen<{ running_count: number; busy: boolean }>("launcher:exit-blocked", ({ payload }) => {
      const count = Math.max(1, payload.running_count);
      const plural = count === 1 ? "browser is" : "browsers are";
      void confirmModal({
        title: payload.busy ? tr("Operation in progress") : tr("Close running browsers first"),
        message: payload.busy ? tr("Wait for storage or runtime operations to finish, and resolve incomplete browser recovery before exiting.") : tr("ShardX cannot exit while {0} launched {1} still running. Close {2} first, then try again.", count, plural, count === 1 ? "it" : "them"),
        buttons: [{ label: tr("OK"), value: true, primary: true }],
      });
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("shardx-theme", theme);
  }, [theme]);
  return (
    <>
      {/* Custom title bar; drag-region outside .app stays clickable above modals. */}
      <div
        className={`titlebar ${HOST_OS === "macOS" ? "titlebar-mac" : "titlebar-custom"}`}
        data-tauri-drag-region
      >
        <span className="titlebar-title">ShardX Launcher</span>
        {/* Custom min/max/close on Win/Linux (macOS uses native traffic lights). */}
        {HOST_OS !== "macOS" && (
          <div className="titlebar-controls">
            <button
              className="tb-btn"
              aria-label={tr("Minimize")}
              onClick={() => getCurrentWindow().minimize()}
            >
              <Minus size={14} />
            </button>
            <button
              className="tb-btn"
              aria-label={tr("Maximize")}
              onClick={() => getCurrentWindow().toggleMaximize()}
            >
              <Maximize size={13} />
            </button>
            <button
              className="tb-btn tb-close"
              aria-label={tr("Close")}
              onClick={(event) => {
                prepareTitlebarWindowHide(event.currentTarget);
                void getCurrentWindow().close();
              }}
            >
              <X size={14} />
            </button>
          </div>
        )}
      </div>
      <FirstRunGate>
        <div className="app">
          <Sidebar
            section={section}
            onSelect={setSection}
            navigationLocked={workspaceBackup.status === "restore"}
            theme={theme}
            onToggleTheme={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
          />
          <main className="main" key={section}>
            <div className={`workspace-page${section === "browsers" || section === "proxies" ? " workspace-page-wide" : ""}`}>
              {section === "browsers" && <BrowsersView />}
              {section === "proxies" && <ProxiesView />}
              {section === "fingerprints" && <FingerprintsView />}
              {section === "extensions" && <ExtensionsView {...storageViewProps} />}
              {section === "trash" && <TrashView {...storageViewProps} />}
              {section === "settings" && <SettingsView workspaceBackup={workspaceBackup} />}
            </div>
          </main>
          <ToastHost />
          <ConfirmHost />
        </div>
      </FirstRunGate>
    </>
  );
}

function Sidebar({
  section, onSelect, navigationLocked, theme, onToggleTheme,
}: {
  section: Section;
  onSelect: (s: Section) => void;
  navigationLocked: boolean;
  theme: "dark" | "light";
  onToggleTheme: () => void;
}) {
  const sections: { label: string; items: { id: Section; label: string; svg: ReactNode }[] }[] = [
    {
      label: tr("Workspace"),
      items: [
        { id: "browsers", label: tr("Browsers"), svg: <AppWindow size={18} /> },
        { id: "proxies", label: tr("Proxies"), svg: <Network size={18} /> },
        { id: "trash", label: tr("Recycle bin"), svg: <Trash size={18} /> },
      ],
    },
    {
      label: tr("Library"),
      items: [
        { id: "fingerprints", label: tr("Fingerprints"), svg: <FingerprintPattern size={18} /> },
        { id: "extensions", label: tr("Extensions"), svg: <Puzzle size={18} /> },
      ],
    },
    {
      label: tr("System"),
      items: [{ id: "settings", label: tr("Settings"), svg: <SettingsIcon size={18} /> }],
    },
  ];

  // Automation/MCP quick widget (fills the sidebar's lower space).
  const [autoUrl, setAutoUrl] = useState("");
  const [mcpBusy, setMcpBusy] = useState(false);
  const refreshListener = async () => {
    try { const info = await invoke<ApiInfo>("api_info"); setAutoUrl(info.listener.state === "listening" ? `http://127.0.0.1:${info.listener.port}` : ""); }
    catch (e) { toast.err(String(e)); }
  };
  useEffect(() => { void refreshListener(); }, []);
  useStoreChanged(refreshListener, ["api"]);
  const downloadMcp = async () => {
    setMcpBusy(true);
    try {
      const p = await invoke<string>("mcp_download");
      toast.ok(tr("MCP downloaded to {0}", p));
    } catch (e) { toast.err(tr("MCP download failed: ") + String(e)); }
    finally { setMcpBusy(false); }
  };

  return (
    <aside className="sidebar">
      <div className="brand">
        <ShardLogo />
        <span>ShardX</span>
      </div>
      <nav>
        {sections.map((sec) => (
          <div key={sec.label} className="nav-group">
            <div className="nav-group-label">{sec.label}</div>
            {sec.items.map((it) => (
              <button
                key={it.id}
                className={`nav-item ${section === it.id ? "active" : ""}`}
                aria-current={section === it.id ? "page" : undefined}
                disabled={navigationLocked && section !== it.id}
                onClick={() => onSelect(it.id)}
              >
                <span className="nav-icon">{it.svg}</span>
                <span>{it.label}</span>
                {section === it.id && <span className="nav-active-dot" />}
              </button>
            ))}
          </div>
        ))}
      </nav>
      <div className="sidebar-foot">
        <div className="side-auto">
          <div className="side-auto-head">{tr("Automation API")}</div>
          {autoUrl ? (
            <button
              className="side-auto-url"
              title={tr("Copy API base URL")}
              onClick={() => { clip.write(autoUrl); toast.ok(tr("Copied API URL")); }}
            >
              <span className="mono">{autoUrl.replace(/^https?:\/\//, "")}</span>
              <Icon.Clone size={16} />
            </button>
          ) : (
            <div className="side-auto-off">{tr("API is not listening")}</div>
          )}
          <button className="side-auto-btn" onClick={downloadMcp} disabled={mcpBusy}>
            <Icon.Download size={16} /> {mcpBusy ? tr("Downloading…") : tr("Download MCP")}
          </button>
          <button
            className="side-auto-btn"
            onClick={() => {
              openUrl(withUtm("https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true")).catch(() => {});
            }}
            title={tr("Open the full Automation API reference on docs.proxyshard.com")}
          >
            <Icon.Info size={16} />  {tr("Documentation")} </button>
        </div>
        <button
          className="theme-toggle"
          onClick={onToggleTheme}
          title={theme === "dark" ? tr("Switch to light theme") : tr("Switch to dark theme")}
        >
          <span className={`theme-seg ${theme === "light" ? "active" : ""}`}>
            <Sun size={16} />  {tr("Light")} </span>
          <span className={`theme-seg ${theme === "dark" ? "active" : ""}`}>
            <Moon size={16} />  {tr("Dark")} </span>
        </button>
        <VersionPill />
      </div>
    </aside>
  );
}

// ---- logos / icons ----

function ShardLogo() {
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" className="shard-logo">
      <defs>
        <linearGradient id="g1" x1="0" x2="1" y1="0" y2="1">
          <stop offset="0%" stopColor="#a78bfa" />
          <stop offset="100%" stopColor="#7c3aed" />
        </linearGradient>
      </defs>
      <path d="M11 1L21 11L11 21L1 11Z" fill="url(#g1)" />
      <path d="M11 6L16 11L11 16L6 11Z" fill="#0c0e13" />
    </svg>
  );
}
/// Read-only value with inline copy glyph.
function CopyField({ value, secret }: { value: string; secret?: boolean }) {
  return (
    <div className="copy-field">
      <input readOnly type={secret ? "password" : "text"} value={value} />
      <button
        type="button"
        className="copy-icon"
        title={tr("Copy")}
        onClick={async () => { try { await clip.write(value); toast.ok(tr("Copied")); } catch (e) { toast.err(String(e)); } }}
      >
        <Copy size={16} />
      </button>
    </div>
  );
}
const Icon = {
  Edit: Pencil, Clone: CopyPlus, More: EllipsisVertical, Trash, Refresh: RefreshCw,
  Grip: GripVertical, Loader: LoaderCircle, Info, Folder, Upload, Download,
  Globe, Clock, Building: Building2, Stop: Square, Play,
};

type SortKind = "profile" | "proxy";
type SortPlacement = "before" | "after";

const listPointerCollision: CollisionDetection = (args) => {
  const pointer = args.pointerCoordinates;
  if (!pointer) return [];
  const items: string[] = args.active.data.current?.sortable?.items ?? [];

  // Untransformed row rects keep the insertion thresholds steady as rows move.
  // Hit-test the container as well: wide rows can extend behind the sidebar.
  const underPointer = document.elementFromPoint(pointer.x, pointer.y);
  const targetType = underPointer?.closest(".folder-tabs") ? "profile-folder"
    : underPointer?.closest(".rows") ? "sortable-row" : null;
  if (!targetType) return [];
  const [hit] = pointerWithin({
    ...args,
    droppableContainers: args.droppableContainers.filter(({ data }) => data.current?.type === targetType),
  });
  if (!hit) return [];
  if (targetType === "profile-folder" || hit.id === args.active.id) return [hit];

  const activeIndex = items.indexOf(String(args.active.id));
  const hitIndex = items.indexOf(String(hit.id));
  const rect = args.droppableRects.get(hit.id);
  if (!rect || activeIndex < 0 || hitIndex < 0) return [];
  const midpoint = rect.top + rect.height / 2;
  const index = hitIndex > activeIndex && pointer.y < midpoint ? hitIndex - 1
    : hitIndex < activeIndex && pointer.y > midpoint ? hitIndex + 1 : hitIndex;
  return [{ id: items[index] }];
};

function useListSort() {
  const keyboardTarget = useRef<UniqueIdentifier | null>(null);
  const coordinateGetter: KeyboardCoordinateGetter = (event, { context, currentCoordinates }) => {
    if (event.code !== "ArrowUp" && event.code !== "ArrowDown") return;
    event.preventDefault();
    const { active, droppableRects, collisionRect } = context;
    const items: string[] = active?.data.current?.sortable?.items ?? [];
    const currentIndex = items.indexOf(String(keyboardTarget.current ?? active?.id));
    if (currentIndex < 0 || !collisionRect) return;
    const nextId = items[currentIndex + (event.code === "ArrowDown" ? 1 : -1)];
    const next = nextId === undefined ? undefined : droppableRects.get(nextId);
    if (!next) return;
    keyboardTarget.current = nextId;
    // Coordinates scroll the target into view; the target ID resolves the drop.
    return { x: currentCoordinates.x, y: next.top + (next.height - collisionRect.height) / 2 };
  };
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter, scrollBehavior: "auto" }),
  );
  const collisionDetection: CollisionDetection = (args) => {
    if (args.pointerCoordinates) return listPointerCollision(args);
    const target = keyboardTarget.current ?? args.active.id;
    return args.droppableRects.has(target) ? [{ id: target }] : [];
  };
  return { sensors, collisionDetection, onDragStart: () => { keyboardTarget.current = null; } };
}

function moveByAnchor<T extends { id: string }>(
  items: T[],
  id: string,
  anchorId: string,
  placement: SortPlacement,
): T[] {
  if (id === anchorId) return items;
  const sourceIndex = items.findIndex((item) => item.id === id);
  const anchorIndex = items.findIndex((item) => item.id === anchorId);
  if (sourceIndex < 0 || anchorIndex < 0) return items;
  const next = [...items];
  const [moving] = next.splice(sourceIndex, 1);
  const shiftedAnchorIndex = next.findIndex((item) => item.id === anchorId);
  next.splice(shiftedAnchorIndex + (placement === "after" ? 1 : 0), 0, moving);
  return next;
}

function listDrop<T extends { id: string }>(event: DragEndEvent, items: T[], kind: SortKind) {
  const { active, over } = event;
  if (over?.data.current?.type !== "sortable-row" || over.data.current.kind !== kind) return null;
  const id = String(active.id);
  const anchorId = String(over.id);
  const from = items.findIndex((item) => item.id === id);
  const to = items.findIndex((item) => item.id === anchorId);
  if (from < 0 || to < 0 || from === to) return null;
  // Same destination index as verticalListSortingStrategy uses for the placeholder.
  const placement: SortPlacement = from < to ? "after" : "before";
  return { id, anchorId, placement };
}

function SortDragLabel({ label }: { label: string }) {
  return <span className="sort-drag-label"><Icon.Grip size={16} /><span>{label}</span></span>;
}

function ListSortOverlay() {
  const { active, over } = useDndContext();
  const label = active?.data.current?.label;
  // Keep the overlay mounted for dnd-kit's row measurement. Only show a
  // floating label when there is no row placeholder to carry it.
  return (
    <DragOverlay dropAnimation={null} style={{ pointerEvents: "none" }}>
      {label && over?.data.current?.type !== "sortable-row" && (
        <div className="sort-overlay" aria-hidden="true"><SortDragLabel label={label} /></div>
      )}
    </DragOverlay>
  );
}

function SortableRow({
  id,
  kind,
  name,
  disabledReason,
  className,
  rowClassName,
  onContextMenu,
  children,
  footer,
}: {
  id: string;
  kind: SortKind;
  name: string;
  disabledReason?: string;
  className: string;
  rowClassName: string;
  onContextMenu?: (event: ReactMouseEvent<HTMLDivElement>) => void;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const disabled = !!disabledReason;
  const label = name || id.slice(0, 8);
  const {
    attributes,
    listeners,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition,
    isDragging,
    overIndex,
  } = useSortable({
    id,
    disabled,
    data: { type: "sortable-row", kind, label },
  });
  const placeholder = isDragging && overIndex >= 0;
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
  };
  return (
    <div
      ref={setNodeRef}
      style={style}
      className={`${className}${placeholder ? " row-sort-placeholder" : isDragging ? " row-sort-source" : ""}`}
      onContextMenu={onContextMenu}
    >
      <div className={rowClassName}>
        <div className="sort-handle-cell">
          <button
            ref={setActivatorNodeRef}
            type="button"
            className="sort-handle"
            disabled={disabled}
            title={disabledReason || tr("Drag to reorder · Space + arrow keys also work")}
            aria-label={disabledReason || tr("Reorder {0}", kind)}
            {...attributes}
            {...listeners}
          >
            <Icon.Grip size={16} />
          </button>
        </div>
        {children}
      </div>
      {footer}
      {placeholder && (
        <div className="sort-placeholder-preview" aria-hidden="true"><SortDragLabel label={label} /></div>
      )}
    </div>
  );
}

function FolderDropTab({
  dropId,
  folder,
  className,
  title,
  onClick,
  onContextMenu,
  children,
}: {
  dropId: string;
  folder: string;
  className: string;
  title?: string;
  onClick: () => void;
  onContextMenu?: (event: ReactMouseEvent<HTMLButtonElement>) => void;
  children: ReactNode;
}) {
  const { setNodeRef, isOver } = useDroppable({
    id: dropId,
    data: { type: "profile-folder", folder },
  });
  return (
    <button
      ref={setNodeRef}
      className={`${className} ${isOver ? "folder-tab-drop" : ""}`}
      title={title}
      onClick={onClick}
      onContextMenu={onContextMenu}
    >
      {children}
    </button>
  );
}

// ---- Browsers view ----

function BrowsersView() {
  const [profiles, setProfiles] = useState<ProfileMeta[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [proxies, setProxies] = useState<ProxyEntry[]>([]);
  const [proxySnapshots, setProxySnapshots] = useState<Record<string, ProxyTestSnapshot>>({});
  const [search, setSearch] = useState("");
  const [folder, setFolder] = useState("all");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<ProfileForm | null>(null);
  // Values captured when the inline editor opened. If the notes/proxy dialog
  // saves while the editor is open, an untouched field keeps the dialog's
  // change instead of the editor's stale snapshot overwriting it.
  const [draftBaseline, setDraftBaseline] = useState<ProfileForm | null>(null);
  const draftStoredBaseline = useRef<any>(null);
  const saveInFlight = useRef(false);
  const [saveBusy, setSaveBusy] = useState(false);
  const backupInFlight = useRef(false);
  const [backupBusy, setBackupBusy] = useState(false);
  const [operations, setOperations] = useState<{ profiles: string[]; runtime: boolean; jobs: number }>({ profiles: [], runtime: false, jobs: 0 });
  const [recovery, setRecovery] = useState<{ incomplete: boolean; error: string | null; attempts: number } | null>(null);
  const [recovering, setRecovering] = useState(false);
  const refreshRecovery = async () => { try { setRecovery(await invoke("process_recovery_status")); } catch (e) { toast.err(String(e)); } };
  useEffect(() => { void refreshRecovery(); }, []);
  useStoreChanged(refreshRecovery, ["recovery"]);
  const retryRecovery = async () => {
    setRecovering(true);
    try { await invoke("process_recovery_retry"); await reload(); }
    catch (e) { toast.err(String(e)); }
    finally { await refreshRecovery(); setRecovering(false); }
  };
  // Value = epoch ms at which the engine was first observed running. Used
  // both as a truthy flag (any number = running) and as the anchor for the
  // ticking uptime display in the Status column.
  const [running, setRunning] = useState<Record<string, number>>({});
  const [backendActiveIds, setBackendActiveIds] = useState<Set<string>>(new Set());
  const processPollRevision = useRef(0);
  // Re-render trigger so the uptime label ticks every second without
  // re-fetching the process list (which polls every 2s).
  const [, setUptimeTick] = useState(0);
  useEffect(() => {
    if (Object.keys(running).length === 0) return;
    const h = setInterval(() => setUptimeTick((t) => t + 1), 1000);
    return () => clearInterval(h);
  }, [running]);
  const [startBusy, setStartBusy] = useState<Set<string>>(new Set());
  const queuedLaunches = useRef(new Set<string>());
  const [assigning, setAssigning] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [renaming, setRenaming] = useState<{ id: string; draft: string } | null>(null);
  const [fingerprints, setFingerprints] = useState<FingerprintEntry[]>([]);
  const [quickEdit, setQuickEdit] = useState<{ kind: "proxy" | "notes"; profile: ProfileMeta } | null>(null);
  // A profile may be launched while one of its editors is already open.
  // Close stale editors as soon as launch starts; the backend guard remains
  // authoritative for launches initiated outside this view.
  useEffect(() => {
    if (expanded && expanded !== "__new__" && (running[expanded] || startBusy.has(expanded) || backendActiveIds.has(expanded))) {
      setExpanded(null);
      setDraft(null);
    }
    if (quickEdit && (running[quickEdit.profile.id] || startBusy.has(quickEdit.profile.id) || backendActiveIds.has(quickEdit.profile.id))) setQuickEdit(null);
    if (renaming && (running[renaming.id] || startBusy.has(renaming.id) || backendActiveIds.has(renaming.id))) setRenaming(null);
  }, [running, startBusy, backendActiveIds, expanded, quickEdit, renaming]);
  // Empty folders persist in localStorage until a profile lands in them.
  const folderRegistry = useWorkspaceFolders();
  const [folderModal, setFolderModal] = useState<{ profileId: string | null } | null>(null);
  const sort = useListSort();
  const rememberFolder = (f: string) => updateWorkspaceFolders((r) => r.includes(f) ? r : [...r, f]);
  const forgetFolder = (f: string) => updateWorkspaceFolders((r) => r.filter((x) => x !== f));
  const ctx = useContextMenu();

  const reload = async () => {
    try {
      setProfiles(await invoke<ProfileMeta[]>("profile_list"));
      setProxies(await invoke<ProxyEntry[]>("proxy_list"));
      setLoadError(null);
    } catch (e) {
      setLoadError(String(e));
    } finally { setLoading(false); }
  };
  useEffect(() => { reload(); }, []);
  // Pick up profiles/proxies created via the automation API or MCP live.
  useStoreChanged(reload, ["profiles", "proxies", "trash"]);
  useEffect(() => {
    invoke<FingerprintEntry[]>("fingerprint_list").then(setFingerprints).catch((e) => toast.err(String(e)));
  }, []);

  // Scroll the expanded editor into view after expand animation.
  useEffect(() => {
    if (!expanded || expanded === "__new__") return;
    const t = setTimeout(() => {
      const el = document.querySelector<HTMLElement>(".row-wrap.row-expanded .inline-editor");
      el?.scrollIntoView({ behavior: "smooth", block: "center" });
    }, 60);
    return () => clearTimeout(t);
  }, [expanded]);

  // 2s poll for real child status; not optimistic UI state.  Uptime is
  // anchored to the moment the engine actually started (now - uptime_ms),
  // preserved across polls so the displayed clock doesn't jitter.  When a
  // profile transitions running → not-running, the backend has just bumped
  // its persisted `total_runtime_ms` — re-fetch profile_list so the Time
  // column reflects the new total (otherwise it shows whatever was on
  // disk before this session started, looking like a "reset").
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      const revision = processPollRevision.current;
      try {
        const [list, activeIds, operationState] = await Promise.all([
          invoke<{ profile_id: string; pid: number; uptime_ms: number }[]>("process_list"),
          invoke<string[]>("profile_active_ids"),
          invoke<typeof operations>("operation_status"),
        ]);
        if (cancelled || revision !== processPollRevision.current) return;
        setBackendActiveIds(new Set(activeIds));
        setOperations(operationState);
        const now = Date.now();
        setRunning((prev) => {
          const next: Record<string, number> = {};
          for (const r of list) {
            next[r.profile_id] = prev[r.profile_id] ?? (now - r.uptime_ms);
          }
          // Detect any profile that was running on the previous tick but
          // dropped off this one — those need a profile_list refresh so
          // the freshly-accumulated total_runtime_ms appears in the UI.
          const justExited = Object.keys(prev).some((id) => !(id in next));
          if (justExited) {
            // Defer to next tick so React commits `next` before reload()
            // races against it.
            setTimeout(() => { if (!cancelled) reload(); }, 0);
          }
          return next;
        });
      } catch {}
    };
    tick();
    const handle = setInterval(tick, 2000);
    return () => { cancelled = true; clearInterval(handle); };
  }, []);

  const proxyMap = useMemo(() => Object.fromEntries(proxies.map((p) => [p.id, p])), [proxies]);

  // Folder tabs derived from profile assignments; "all" always first.
  const folders = useMemo(() => {
    const set = new Set<string>(folderRegistry);
    for (const p of profiles) if (p.folder) set.add(p.folder);
    return [...set].sort((a, b) => a.localeCompare(b));
  }, [profiles, folderRegistry]);

  const visible = useMemo(
    () =>
      profiles.filter(
        (p) =>
          (folder === "all" || p.folder === folder) &&
          p.name.toLowerCase().includes(search.toLowerCase()),
      ),
    [profiles, search, folder],
  );

  // Native non-passive wheel handler turns vertical scroll into horizontal tab scroll.
  const folderTabsRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = folderTabsRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      if (el.scrollWidth <= el.clientWidth || e.deltaY === 0) return;
      e.preventDefault();
      el.scrollLeft += e.deltaY;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  // Reuse the most recent persisted test for every saved proxy. Browser rows
  // consume their bound proxy's snapshot, while the bind-proxy dialog can
  // enrich every option with its exit IP and real location. This only reads
  // local history; opening the page never contacts a geo service.
  useEffect(() => {
    if (proxies.length === 0) {
      setProxySnapshots({});
      return;
    }
    let cancelled = false;
    (async () => {
      const entries = await Promise.all(
        proxies.map(async ({ id }) => {
          try {
            const snap = await invoke<ProxyTestSnapshot | null>("proxy_last_test", { id });
            return [id, snap] as const;
          } catch {
            return [id, null] as const;
          }
        }),
      );
      if (cancelled) return;
      const next: Record<string, ProxyTestSnapshot> = {};
      for (const [id, snap] of entries) if (snap) next[id] = snap;
      setProxySnapshots(next);
    })();
    return () => { cancelled = true; };
  }, [proxies]);

  // Fall back to "all" when the active folder tab becomes empty.
  useEffect(() => {
    if (folder !== "all" && !folders.includes(folder)) setFolder("all");
  }, [folders, folder]);

  const runningCount = Object.values(running).filter(Boolean).length;
  const selectedHasRunning = [...selected].some((id) => !!running[id]);
  const selectedHasStarting = [...selected].some((id) => startBusy.has(id) || (backendActiveIds.has(id) && !running[id]));
  const selectedHasActive = selectedHasRunning || selectedHasStarting || operations.runtime || !!recovery?.incomplete || [...selected].some(id => operations.profiles.includes(id));

  const clearFailedLaunch = (id: string) => {
    // Discard polls started before the failure, which can still report the
    // released backend reservation as active.
    processPollRevision.current += 1;
    setBackendActiveIds((current) => {
      const next = new Set(current);
      next.delete(id);
      return next;
    });
  };

  const cancelStart = async (id: string) => {
    if (queuedLaunches.current.delete(id)) {
      setStartBusy((current) => new Set([...current].filter((value) => value !== id)));
      toast.info(tr("Launch cancelled"));
      return;
    }
    try {
      const accepted = await invoke<boolean>("launch_cancel", { profileId: id });
      if (!accepted) toast.info(tr("The browser is already starting its process; use Stop once it is running"));
      // Keep the profile locked until the launch future has actually unwound.
    } catch (e) { toast.err(String(e)); }
  };

  // Keep the launch busy until the backend returns; failures unlock immediately.
  const startStop = async (p: ProfileMeta) => {
    if (running[p.id]) {
      try {
        await invoke<boolean>("process_kill", { profileId: p.id });
      } catch (e) {
        toast.err(String(e));
      }
      return;
    }
    if (startBusy.has(p.id) || backendActiveIds.has(p.id)) {
      await cancelStart(p.id);
      return;
    }
    setStartBusy((s) => new Set([...s, p.id]));
    try {
      const res = await invoke<{ pid: number; warnings: string[] }>("launch", { profileId: p.id });
      for (const warning of res.warnings ?? []) toast.warn(warning);
      // Don't optimistically flip `running` here; the 2s poll above picks
      // up the new child immediately and anchors the uptime clock.
    } catch (e) {
      clearFailedLaunch(p.id);
      if (String(e) === "Launch cancelled") toast.info(tr("Launch cancelled"));
      else toast.err(String(e));
    } finally {
      setStartBusy((s) => {
        const n = new Set(s);
        n.delete(p.id);
        return n;
      });
    }
  };

  const remove = async (id: string) => {
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      toast.err(tr("Stop the browser before deleting this profile"));
      return;
    }
    if ((await confirmModal({ title: tr("Move to recycle bin"), message: tr("Move this profile and its browser data to the recycle bin? You can restore it within 7 days."), danger: true })) !== true) return;
    try {
      await invoke("profile_delete", { id });
      setSelected((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const cloneProfile = async (id: string) => {
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      toast.err(tr("Stop the browser before cloning this profile"));
      return;
    }
    try {
      await invoke<ProfileMeta>("profile_clone", { id });
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const exportProfiles = async (ids: string[]) => {
    if (backupInFlight.current) return;
    if (ids.length === 0) return;
    const activeIds = ids.filter((id) => running[id] || startBusy.has(id) || backendActiveIds.has(id));
    if (activeIds.length > 0) {
      toast.err(tr("Stop the {0} active browser{1} before exporting", activeIds.length, activeIds.length === 1 ? "" : "s"));
      return;
    }
    backupInFlight.current = true;
    setBackupBusy(true);
    try {
      const summary = await invoke<ProfileBackupSummary>("profile_backup_export", { profileIds: ids });
      // The backup intentionally carries the Chromium os-crypt key so it can
      // be restored for another Windows user — which means anyone holding the
      // file can decrypt every session cookie inside it. Say so loudly.
      toast.warn(
        tr("Exported {0} profile backup{1} ({2} files), excluding rebuildable caches. The backup contains the browser's cookie-decryption key: anyone with this file can sign in as these accounts. Store it offline, never upload it.", summary.profileCount, summary.profileCount === 1 ? "" : "s", summary.fileCount),
      );
      // Backups are written only inside the launcher's fixed portable exports
      // directory; no arbitrary destination path crosses the trust boundary.
      await invoke("open_exports_dir");
    } catch (e) { toast.err(String(e)); }
    finally { backupInFlight.current = false; setBackupBusy(false); }
  };

  // Per-profile action menu shared by right-click and ⋮ button.
  const profileMenu = (p: ProfileMeta) => [
    { label: running[p.id] ? tr("Stop") : startBusy.has(p.id) || backendActiveIds.has(p.id) ? tr("Cancel launch") : tr("Launch"), onClick: () => startStop(p) },
    { label: tr("Edit"), onClick: () => expand(p.id) },
    { label: tr("Clone"), onClick: () => cloneProfile(p.id) },
    { sep: true, label: "", onClick: () => {} },
    { label: tr("Move to folder…"), onClick: () => setFolderModal({ profileId: p.id }) },
    ...(p.folder
      ? [{ label: tr("Remove from folder"), onClick: () => setProfileFolder(p.id, "") }]
      : []),
    { sep: true, label: "", onClick: () => {} },
    {
      label: tr("Export profile"),
      onClick: () => exportProfiles([p.id]),
      title: tr("Back up persistent browser data, fingerprint, extensions, and bound proxy; exclude rebuildable caches"),
    },
    { sep: true, label: "", onClick: () => {} },
    { label: tr("Delete"), onClick: () => remove(p.id), danger: true },
  ];

  const beginRename = (p: ProfileMeta) => {
    if (running[p.id] || startBusy.has(p.id) || backendActiveIds.has(p.id)) return;
    if (expanded === p.id) {
      setExpanded(null);
      setDraft(null);
    }
    setRenaming({ id: p.id, draft: p.name });
  };

  const commitRename = async (id: string, value: string) => {
    const closeEditor = () => setRenaming((current) => current?.id === id ? null : current);
    const profile = profiles.find((p) => p.id === id);
    if (!profile) {
      closeEditor();
      return;
    }
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      closeEditor();
      toast.err(tr("Stop the browser or wait for it to finish starting before renaming"));
      return;
    }

    if (value === profile.name) {
      closeEditor();
      return;
    }
    const error = displayNameError(value);
    if (error) { toast.err(error); return; }

    try {
      await invoke("profile_rename", { id, name: value });
      closeEditor();
      await reload();
    } catch (e) {
      toast.err(String(e));
    }
  };

  const setProfileFolder = async (id: string, f: string) => {
    // Dropping a profile onto the folder it already lives in is a no-op —
    // tell the user instead of silently doing nothing.
    const p = profiles.find((x) => x.id === id);
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      toast.err(tr("Stop the browser before moving this profile"));
      return;
    }
    if (p && p.folder === f) {
      const who = p.name || id.slice(0, 8);
      toast.info(f ? tr("“{0}” is already in “{1}”", who, f) : tr("“{0}” isn’t in any folder", who));
      return;
    }
    try {
      await invoke("profile_set_folder", { id, folder: f });
      if (f) rememberFolder(f);
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const deleteFolder = async (f: string) => {
    const count = profiles.filter((p) => p.folder === f).length;
    const runningCount = profiles.filter((p) => p.folder === f && running[p.id]).length;
    if (runningCount > 0) {
      toast.err(tr("Stop the {0} running browser{1} in this folder first", runningCount, runningCount === 1 ? "" : "s"));
      return;
    }
    // Three outcomes: recycle profiles, unfile, cancel.
    const choice = await confirmModal({
      title: tr("Delete folder “{0}”", f),
      message:
        count > 0
          ? tr("This folder has {0} profile{1}. ", count, count === 1 ? "" : "s") +
            tr("Move them to the recycle bin for 7 days, or keep them in All?")
          : tr("Delete the empty folder “{0}”?", f),
      buttons:
        count > 0
          ? [
              { label: tr("Cancel"), value: "cancel" },
              { label: tr("Keep profiles"), value: "keep" },
              { label: tr("Move to recycle bin"), value: "delete", danger: true },
            ]
          : [
              { label: tr("Cancel"), value: "cancel" },
              { label: tr("Delete"), value: "keep", danger: true },
            ],
    });
    if (choice == null || choice === "cancel") return;
    const alsoDelete = choice === "delete";
    const deletedIds = alsoDelete
      ? profiles.filter((p) => p.folder === f).map((p) => p.id)
      : [];
    try {
      const n = await invoke<number>("folder_delete", { folder: f, deleteProfiles: alsoDelete });
      // The folder lives in two places: profile tags (cleared by folder_delete)
      // and the localStorage registry of empty folders.  Drop it from the
      // registry too, otherwise the tab lingers after every profile is gone.
      forgetFolder(f);
      if (deletedIds.length > 0) {
        setSelected((prev) => new Set([...prev].filter((id) => !deletedIds.includes(id))));
      }
      if (folder === f) setFolder("all");
      reload();
      toast.ok(
        alsoDelete
          ? tr("Deleted folder “{0}”; moved {1} profiles to the recycle bin", f, n)
          : tr("Removed folder “{0}” ({1} profile{2} kept)", f, n, n === 1 ? "" : "s"),
      );
    } catch (e) { toast.err(String(e)); }
  };

  const bulkLaunch = async () => {
    const ids = [...selected];
    if (ids.length === 0) return;
    const runningIds = ids.filter((id) => !!running[id]);
    if (runningIds.length > 0) {
      toast.err(tr("Stop the {0} selected running browser{1} before launching", runningIds.length, runningIds.length === 1 ? "" : "s"));
      return;
    }
    const startingIds = ids.filter((id) => startBusy.has(id) || (backendActiveIds.has(id) && !running[id]));
    if (startingIds.length > 0) {
      toast.err(tr("Wait for the selected browser to finish starting"));
      return;
    }

    setStartBusy((state) => new Set([...state, ...ids]));
    for (const id of ids) queuedLaunches.current.add(id);
    for (const id of ids) {
      if (!queuedLaunches.current.delete(id)) continue;
      try {
        const res = await invoke<{ pid: number; warnings: string[] }>("launch", { profileId: id });
        for (const warning of res.warnings ?? []) toast.warn(warning);
      } catch (e) {
        clearFailedLaunch(id);
        if (String(e) === "Launch cancelled") toast.info(tr("Launch cancelled"));
        else toast.err(tr("Failed to launch {0}: {1}", id.slice(0, 8), e));
      } finally {
        setStartBusy((state) => {
          const next = new Set(state);
          next.delete(id);
          return next;
        });
      }
    }
    setSelected(new Set());
  };

  const bulkStop = async () => {
    const ids = [...selected];
    if (ids.length === 0) return;
    const stopped = new Set<string>();
    for (const id of ids) {
      try {
        await invoke<boolean>("process_kill", { profileId: id });
        stopped.add(id);
      } catch (e) {
        toast.err(tr("Failed to stop {0}: {1}", id.slice(0, 8), e));
      }
    }
    // Keep profiles that failed to stop selected so the batch context
    // survives; the 2s poll keeps showing their real running state.
    setSelected((prev) => new Set([...prev].filter((id) => !stopped.has(id))));
  };

  const bulkDelete = async () => {
    const ids = [...selected];
    if (ids.length === 0) return;
    const activeIds = ids.filter((id) => !!running[id] || startBusy.has(id) || backendActiveIds.has(id));
    if (activeIds.length > 0) {
      toast.err(tr("Stop the {0} selected active browser{1} first", activeIds.length, activeIds.length === 1 ? "" : "s"));
      return;
    }
    if ((await confirmModal({ title: tr("Move to recycle bin"), message: tr("Move {0} profiles and their browser data to the recycle bin for 7 days?", ids.length), danger: true })) !== true) return;
    try {
      const count = await invoke<number>("profile_trash_many", { ids });
      setSelected(new Set());
      await reload();
      toast.ok(tr("Moved {0} profiles to the recycle bin", count));
    } catch (e) { toast.err(String(e)); }
  };

  const bulkExport = async () => {
    const ids = [...selected];
    await exportProfiles(ids);
  };

  const bulkImport = async () => {
    if (backupInFlight.current) return;
    backupInFlight.current = true;
    setBackupBusy(true);
    try {
      const paths = await pickProfileBackupFiles();
      if (!paths) return;
      const summary = await invoke<ProfileBackupSummary>("profile_backup_import", { paths });
      await reload();
      toast.ok(
        tr("Restored {0} profile{1}, including bound proxy settings ({2} browser-data files)", summary.profileCount, summary.profileCount === 1 ? "" : "s", summary.fileCount),
      );
    } catch (e) {
      toast.err(tr("Import failed: ") + String(e));
    } finally { backupInFlight.current = false; setBackupBusy(false); }
  };

  const expand = async (id: string) => {
    if (running[id]) {
      toast.err(tr("Stop the browser before editing this profile"));
      return;
    }
    setRenaming(null);
    setQuickEdit(null);
    if (expanded === id) { setExpanded(null); setDraft(null); setDraftBaseline(null); return; }
    try {
      const stored = await invoke<any>("profile_get", { id });
      draftStoredBaseline.current = stored;
      setDraft(fromStored(stored));
      setDraftBaseline(fromStored(stored));
      setExpanded(id);
    } catch (e) { toast.err(String(e)); }
  };

  const newProfile = async () => {
    setRenaming(null);
    setDraft(defaultForm());
    setDraftBaseline(null);
    setExpanded("__new__");
  };

  const nameError = draft ? displayNameError(draft.name, draftBaseline?.name) : null;
  const saveDraft = async () => {
    if (!draft || saveInFlight.current) return;
    if (nameError) {
      toast.err(nameError);
      return;
    }
    saveInFlight.current = true;
    setSaveBusy(true);
    try {
      const fp = fingerprints.find((g) => g.id === draft.gpu_preset_id) ?? null;
      // Preserve the durable config when editing an existing profile. In
      // particular, its screen/window block was clamped to this display when
      // the profile was created and must not be replaced by donor dimensions
      // merely because NAME (or another unrelated field) changed.
      const existing = draft.id
        ? await invoke<any>("profile_get", { id: draft.id })
        : null;
      // Merge unchanged editor fields from the latest stored profile, including
      // extension choices changed through the API while this editor was open.
      const edited = toStored(draft, fp, draft.id ? draftStoredBaseline.current : null, draftBaseline);
      let payload = edited;
      if (draft.id) {
        try { payload = mergeProfileEdits(draftStoredBaseline.current, edited, existing); }
        catch (e) { throw tr("These fields changed elsewhere: {0}. Reopen the editor to review the latest values.", e instanceof Error ? e.message : String(e)); }
      }
      if (!draft.id && folder !== "all") payload._meta.folder = folder;
      const saved = await invoke<ProfileMeta>("profile_save", { payload });
      setExpanded(null);
      setDraft(null);
      setDraftBaseline(null);
      reload();
      toast.ok(draft.id ? tr("Profile saved") : tr("Created \"{0}\"", saved.name));
    } catch (e) { toast.err(String(e)); }
    finally { saveInFlight.current = false; setSaveBusy(false); }
  };

  const toggleSel = (id: string) => {
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id); else n.add(id);
      return n;
    });
  };

  const handleProfileDragEnd = async (event: DragEndEvent) => {
    const { over } = event;
    if (over?.data.current?.type === "profile-folder") {
      await setProfileFolder(String(event.active.id), String(over.data.current.folder ?? ""));
      return;
    }
    const drop = listDrop(event, visible, "profile");
    if (!drop) return;
    setProfiles(moveByAnchor(profiles, drop.id, drop.anchorId, drop.placement));
    try {
      await invoke("profile_move_order", drop);
    } catch (error) {
      // Reload instead of restoring the stale snapshot: a store-changed
      // reload may have landed while the move was in flight (e.g. an MCP
      // import), and overwriting it would drop those rows until the next
      // poll.
      await reload();
      toast.err(tr("Could not save profile order: {0}", String(error)));
    }
  };

  return (
    <DndContext
      {...sort}
      onDragEnd={handleProfileDragEnd}
    >
    <section className="page">
      <PageToolbar title={tr("Browsers")} search={{ value: search, onChange: setSearch }}>
        <button className="btn-ghost" disabled={backupBusy || operations.runtime} onClick={bulkImport} title={tr("Import one or more .shardx-backup files")}><Icon.Download size={16} /> {tr("Import profile")}</button>
        <button className="btn-primary" disabled={saveBusy || operations.runtime || !!recovery?.incomplete} onClick={newProfile}><Plus /> {tr("New profile")}</button>
      </PageToolbar>

      <dl className="metric-strip">
        <Metric label={tr("Profiles")} value={String(profiles.length)} accent />
        <Metric label={tr("Running")} value={String(runningCount)} running={runningCount > 0} />
        <Metric label={tr("Proxies")} value={String(proxies.length)} />
        <Metric label={tr("Fingerprints")} value={String(fingerprints.length)} />
      </dl>

        <div className="folder-tabs" ref={folderTabsRef}>
            <FolderDropTab
              dropId="profile-folder:all"
              folder=""
              className={`folder-tab ${folder === "all" ? "active" : ""}`}
              onClick={() => setFolder("all")}
            >
               {tr("All")}<span className="tab-count">{profiles.length}</span>
            </FolderDropTab>
            {folders.map((f) => (
              <FolderDropTab
                key={f}
                dropId={`profile-folder:${f}`}
                folder={f}
                className={`folder-tab ${folder === f ? "active" : ""}`}
                onClick={() => setFolder(f)}
                title={tr("Right-click for folder actions · drop profiles to move them")}
                onContextMenu={(e) =>
                  ctx.open(e, [
                    { label: tr("Delete folder…"), onClick: () => deleteFolder(f), danger: true },
                  ])
                }
              >
                {f}
                <span className="tab-count">
                  {profiles.filter((p) => p.folder === f).length}
                </span>
              </FolderDropTab>
            ))}
            <button
                 className="folder-tab folder-tab-add"
                 title={tr("Create a new folder")}
                 aria-label={tr("Create a new folder")}
              onClick={() => setFolderModal({ profileId: null })}
            >
               <Plus />
            </button>
      </div>
      {recovery?.incomplete && <div className="operation-banner" role="alert"><div><strong>{tr("Browser recovery needs attention")}</strong><p>{recovery.error}</p></div><button className="btn-ghost" disabled={recovering} onClick={() => void retryRecovery()}>{recovering ? tr("Recovering…") : tr("Retry recovery")}</button></div>}
      {(backupBusy || operations.runtime || operations.profiles.length > 0) && <div className="operation-banner" role="status">{operations.runtime ? tr("Runtime update in progress. Profile changes will be available when it finishes.") : tr("Profile backup in progress. Keep the launcher open until it finishes.")}</div>}
      <SelectionBar count={selected.size}>
          <button
            className="btn-ghost btn-sm"
            onClick={bulkLaunch}
            disabled={selectedHasActive}
            title={selectedHasRunning ? tr("Stop selected running browsers before launching") : selectedHasStarting ? tr("Wait for selected browsers to finish starting") : tr("Launch selected profiles")}
          ><Icon.Play size={16} /> {tr("Launch")}</button>
          <button className="btn-ghost btn-sm" onClick={bulkStop}><Icon.Stop size={16} /> {tr("Stop")}</button>
          {selectedHasStarting && <button className="btn-ghost btn-sm" onClick={() => {
            for (const id of selected) if (startBusy.has(id) || (backendActiveIds.has(id) && !running[id])) void cancelStart(id);
          }}>{tr("Cancel launch")}</button>}
          <button className="btn-ghost btn-sm" disabled={selectedHasActive} onClick={() => setAssigning(true)}>{tr("Assign proxies")}</button>
          <button
            className="btn-ghost btn-sm"
            onClick={bulkExport}
            disabled={selectedHasActive || backupBusy}
            title={selectedHasActive ? tr("Stop selected active browsers before exporting") : tr("Back up selected profiles, excluding rebuildable caches")}
          ><Icon.Upload size={16} /> {tr("Export")}</button>
          <button
            className="btn-ghost btn-sm"
            onClick={bulkDelete}
            disabled={selectedHasActive}
            title={selectedHasActive ? tr("Stop selected active browsers before deleting") : tr("Delete selected profiles")}
          ><Icon.Trash size={16} /> {tr("Delete")}</button>
      </SelectionBar>
      {loadError !== null && <ListState kind="error" text={tr("Unable to load browsers: {0}", loadError)} />}
      {folderModal && (() => {
        const moving = folderModal.profileId
          ? profiles.find((p) => p.id === folderModal!.profileId) ?? null
          : null;
        // "move" mode: pick from other folders; "create" mode: just the input.
        const pickable = moving ? folders.filter((f) => f !== moving.folder) : [];
        const assign = (f: string) => {
          if (folderModal!.profileId) setProfileFolder(folderModal!.profileId, f);
          else rememberFolder(f);
          setFolder(f);
          setFolderModal(null);
        };
        return (
          <FolderModal
            mode={folderModal.profileId ? "move" : "create"}
            existing={pickable}
            onPick={assign}
            onCreate={(name) => { const f = name.trim(); if (f) assign(f); }}
            onClose={() => setFolderModal(null)}
          />
        );
      })()}

      {assigning && <AssignProxiesDialog
        profiles={profiles} proxies={proxies} selected={selected}
        activeIds={new Set([...backendActiveIds, ...startBusy, ...Object.keys(running).filter((id) => running[id])])}
        onClose={() => setAssigning(false)} onSaved={reload}
      />}
      <div className="rows browser-rows">
        <div className="rows-head t-cols">
          <div className="sort-head" title={tr("Drag rows to reorder")}><Icon.Grip size={16} /></div>
          <div>
            <input
              type="checkbox"
              title={tr("Select all matching rows")}
              checked={visible.length > 0 && visible.every((p) => selected.has(p.id))}
              ref={(el) => {
                if (!el) return;
                const anySel = visible.some((p) => selected.has(p.id));
                const allSel = visible.every((p) => selected.has(p.id));
                el.indeterminate = anySel && !allSel;
              }}
              onChange={(e) => {
                setSelected((prev) => {
                  const next = new Set(prev);
                  if (e.target.checked) {
                    for (const p of visible) next.add(p.id);
                  } else {
                    for (const p of visible) next.delete(p.id);
                  }
                  return next;
                });
              }}
            />
          </div>
          <div>{tr("Name")}</div><div className="head-status">{tr("Status")}</div><div className="head-proxy">{tr("Proxy")}</div><div>{tr("Notes")}</div><div className="head-lastrun">{tr("Last run")}</div><div className="head-actions">{tr("Actions")}</div>
        </div>
        {expanded === "__new__" && draft && (
          <div className="row-wrap row-expanded row-new">
            <InlineEditor
              draft={draft}
              nameError={nameError}
              setDraft={setDraft}
              proxies={proxies}
              fingerprints={fingerprints}
              onSave={saveDraft}
              busy={saveBusy}
              onCancel={() => { setExpanded(null); setDraft(null); }}
            />
          </div>
        )}
        <SortableContext items={visible.map((profile) => profile.id)} strategy={verticalListSortingStrategy}>
        {visible.map((p) => {
          const px = p.proxy_id ? proxyMap[p.proxy_id] : null;
          const proxySnapshot = px ? proxySnapshots[px.id] : undefined;
          const proxyCountry = (proxySnapshot?.country_code || px?.country || "").trim().toUpperCase();
          const isRunning = !!running[p.id];
          const isStarting = !isRunning && (startBusy.has(p.id) || backendActiveIds.has(p.id));
          const isReserved = operations.runtime || !!recovery?.incomplete || operations.profiles.includes(p.id);
          const isActive = isRunning || isStarting || isReserved;
          const status = isRunning ? "running" : isReserved ? "busy" : isStarting ? "starting" : "idle";
          const statusLabel = tr({ running: "Running", busy: "Busy", starting: "Starting…", idle: "Idle" }[status]);
          const isExpanded = expanded === p.id;
          const isSel = selected.has(p.id);
          const edit = renaming?.id === p.id ? renaming : null;
          return (
            <SortableRow
              key={p.id}
              id={p.id}
              kind="profile"
              name={p.name}
              className={`row-wrap${isExpanded ? " row-expanded" : ""}`}
              rowClassName="row t-cols browser-data-row"
              disabledReason={isActive ? tr("Stop the browser before reordering this profile") : search.trim() ? tr("Clear search to reorder profiles") : isExpanded ? tr("Close the editor before reordering") : undefined}
              onContextMenu={(e) => {
                if (isActive) {
                  e.preventDefault();
                  return;
                }
                ctx.open(e, profileMenu(p));
              }}
              footer={isExpanded && draft ? (
                <InlineEditor
                  draft={draft}
                  nameError={nameError}
                  setDraft={setDraft}
                  proxies={proxies}
                  fingerprints={fingerprints}
                  onSave={saveDraft}
                  busy={saveBusy}
                  onCancel={() => { setExpanded(null); setDraft(null); }}
                />
              ) : undefined}
            >
                <div>
                  <input type="checkbox" aria-label={p.name} checked={isSel} onChange={() => toggleSel(p.id)} />
                </div>
                <div
                  className={`cell-name ${isActive ? "cell-locked" : ""}`}
                  onClick={isActive || edit ? undefined : () => beginRename(p)}
                  title={isActive ? tr("Stop the browser or wait for it to finish starting before renaming") : tr("Click to rename")}
                >
                  {edit ? (
                    <input
                      autoFocus
                      className="inline-rename profile-inline-rename"
                      value={edit.draft}
                      onChange={(e) => setRenaming({ id: p.id, draft: e.target.value })}
                      onBlur={(e) => commitRename(p.id, e.currentTarget.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") e.currentTarget.blur();
                        else if (e.key === "Escape") setRenaming(null);
                      }}
                      aria-label={tr("Rename {0}", p.name)}
                    />
                  ) : <div className="name-main" title={p.name}>{p.name}</div>}
                  <div className="name-sub table-detail">{p.id.slice(0, 8)}</div>
                </div>
                <div className="cell-status" data-state={status} role="img" aria-label={statusLabel} title={statusLabel}>
                  {status === "starting" ? <Icon.Loader size={14} /> : status === "busy" ? <Clock size={14} /> : (
                    <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" focusable="false">
                      <path d="M7 1.5 12.5 7 7 12.5 1.5 7Z" fill={isRunning ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.25" />
                    </svg>
                  )}
                </div>
                <div
                  className={`cell-proxy cell-click ${px ? "" : "cell-proxy-direct"} ${isActive ? "cell-locked" : ""}`}
                  onClick={isActive ? undefined : () => setQuickEdit({ kind: "proxy", profile: p })}
                  title={isActive ? tr("Stop the browser or wait for it to finish starting before changing proxy") : tr("Change proxy")}
                >
                  {px ? (
                    <>
                      <div className="proxy-main">
                        <span className={`badge badge-${px.kind}`}>{px.kind}</span>
                        {proxyCountry && (
                          <>
                            <CountryFlag cc={proxyCountry} />
                            <span className="flag">{proxyCountry}</span>
                          </>
                        )}
                      </div>
                      <ProxyLocationDetail city={proxySnapshot?.city} ip={proxySnapshot?.ip} />
                    </>
                  ) : <span className="muted">{tr("— direct —")}</span>}
                </div>
                <div
                  className={`cell-notes cell-click ${isActive ? "cell-locked" : ""} ${(p.notes || "").includes("\n") ? "notes-multi" : ""}`}
                  title={isActive ? tr("Stop the browser or wait for it to finish starting before editing notes") : p.notes || tr("Click to edit notes")}
                  onClick={isActive ? undefined : () => setQuickEdit({ kind: "notes", profile: p })}
                >
                  {p.notes ? <span className="notes-text">{p.notes}</span> : <span className="muted">—</span>}
                </div>
                <div className="cell-lastrun"><span className="muted">{p.last_launched_at ? fmtTs(p.last_launched_at) : tr("never")}</span></div>
                <div className="row-actions">
                  <button
                    className={`btn-launch ${isRunning ? "btn-launch-stop" : ""}`}
                    onClick={() => startStop(p)}
                    disabled={isReserved && !isRunning}
                    title={
                      isRunning
                        ? tr("Stop browser")
                        : isStarting
                          ? tr("Cancel launch")
                          : tr("Start browser")
                    }
                    aria-label={isRunning ? tr("Stop browser") : isStarting ? tr("Cancel launch") : tr("Start browser")}
                    aria-busy={isStarting}
                  >
                    {isRunning ? <Icon.Stop size={16} /> : isStarting ? <Icon.Loader size={16} /> : <Icon.Play size={16} />}
                  </button>
                  <button className="icon-btn" onClick={() => expand(p.id)} disabled={isActive} title={isActive ? tr("Stop browser before editing") : tr("Edit")}><Icon.Edit size={16} /></button>
                  <button className="icon-btn" onClick={() => cloneProfile(p.id)} disabled={isActive} title={isActive ? tr("Stop browser before cloning") : tr("Clone")}><Icon.Clone size={16} /></button>
                  <button className="icon-btn danger" onClick={() => remove(p.id)} disabled={isActive} title={isActive ? tr("Stop browser before deleting") : tr("Delete")}><Icon.Trash size={16} /></button>
                  <button
                    className="icon-btn"
                    onClick={(e) => { e.stopPropagation(); ctx.open(e, profileMenu(p)); }}
                    disabled={isActive}
                    title={isActive ? tr("Stop browser before using more actions") : tr("More actions")}
                  ><Icon.More size={16} /></button>
                </div>
            </SortableRow>
          );
        })}
        </SortableContext>
        {visible.length === 0 && !expanded && loadError === null && (loading
          ? <ListState kind="loading" text={tr("Loading…")} />
          : <ListState kind="empty" text={tr(search.trim() ? "No matching browsers" : folder !== "all" ? "No browsers in this folder" : "No profiles yet")} />)}
      </div>
      {ctx.node}
      {quickEdit && (
        <QuickEditDialog
          kind={quickEdit.kind}
          profile={quickEdit.profile}
          proxies={proxies}
          proxySnapshots={proxySnapshots}
          onClose={() => setQuickEdit(null)}
          onSaved={() => { setQuickEdit(null); reload(); }}
        />
      )}
    </section>
    <ListSortOverlay />
    </DndContext>
  );
}

function ProxyLocationDetail({ city, ip }: { city?: string; ip?: string }) {
  const name = city?.trim();
  const address = ip?.trim();
  const detail = [name, address].filter(Boolean).join(" · ");
  return (
    <span className="proxy-location-detail muted table-detail" title={detail || undefined}>
      {(name || !address) && <span>{name || "—"}</span>}
      {name && address && <span className="proxy-detail-separator">·</span>}
      {address && <span className="proxy-detail-ip">{address}</span>}
    </span>
  );
}

function proxyBindingLabel(proxy: ProxyEntry, snapshot?: ProxyTestSnapshot) {
  const primary = proxy.name || `${proxy.host}:${proxy.port}`;
  const fallbackTag = proxy.country || proxy.kind;
  if (!snapshot) return `${primary} · ${fallbackTag}`;

  const locationParts = [
    snapshot.city,
    snapshot.region,
    snapshot.country_code || snapshot.country,
  ]
    .map((part) => (part || "").trim())
    .filter((part, index, all) => (
      !!part
      && all.findIndex((value) => value.toLowerCase() === part.toLowerCase()) === index
    ));
  const location = locationParts.join(", ") || fallbackTag;
  const exitIp = (snapshot.ip || "").trim();

  return [primary, location, exitIp ? `IP ${exitIp}` : ""]
    .filter(Boolean)
    .join(" · ");
}

function AssignProxiesDialog({ profiles, proxies, selected, activeIds, onClose, onSaved }: {
  profiles: ProfileMeta[]; proxies: ProxyEntry[]; selected: Set<string>; activeIds: Set<string>;
  onClose: () => void; onSaved: () => void | Promise<void>;
}) {
  const [chosen, setChosen] = useState(new Set<string>());
  const [sharing, setSharing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const targets = profiles.filter((p) => selected.has(p.id));
  const usedBy: Record<string, string[]> = {};
  for (const p of profiles) if (p.proxy_id) (usedBy[p.proxy_id] ??= []).push(p.id);
  const plan = planAssignment(targets.map((p) => p.id), proxies.filter((p) => chosen.has(p.id)).map((p) => p.id), sharing, usedBy);
  const blocked = targets.some((p) => activeIds.has(p.id));
  const proxyName = (id: string | null) => {
    const p = proxies.find((p) => p.id === id);
    return p ? p.name || `${p.host}:${p.port}` : tr("Direct connection");
  };
  const apply = async () => {
    setBusy(true);
    setError("");
    try {
      const applied = await invoke<number>("profile_assign_proxies", {
        pairs: plan.pairs.map((pair) => ({ ...pair, expectedProxyId: profiles.find((p) => p.id === pair.profileId)!.proxy_id })),
        allowSharing: sharing,
      });
      toast.ok(tr("Assigned proxies to {0} profiles", applied));
      onClose();
    } catch (e) {
      setError(tr(String(e)));
    } finally {
      await onSaved();
      setBusy(false);
    }
  };
  return (
    <DialogBackdrop onClose={() => { if (!busy) onClose(); }} dismissOnBackdrop={false}>
      <div className="dialog assign-dialog">
        <header className="dialog-head">
          <h2>{tr("Assign proxies")}</h2>
          <button className="icon-btn" disabled={busy} onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          <p className="muted">{tr("Profiles and proxies are paired in list order. Profiles without a match keep their current binding.")}</p>
          <div className="assign-proxy-list">
            {proxies.length === 0 && <p>{tr("No proxies available")}</p>}
            {proxies.map((p) => <label className="row-inline" key={p.id}>
              <input type="checkbox" checked={chosen.has(p.id)} disabled={busy} onChange={(event) => {
                setChosen((current) => { const next = new Set(current); if (event.target.checked) next.add(p.id); else next.delete(p.id); return next; });
              }} />
              <span>{p.name || `${p.host}:${p.port}`}</span>
              <span className="muted small">{p.kind} · {usedBy[p.id]?.length ?? 0}  {tr("bindings")}</span>
            </label>)}
          </div>
          <label className="row-inline">
            <input type="checkbox" checked={sharing} disabled={busy} onChange={(e) => setSharing(e.target.checked)} />
            <span>{tr("Allow sharing proxies (round-robin)")}</span>
          </label>
          <p className="muted small">{plan.pairs.length}  {tr("assignments ·")} {plan.left.length}  {tr("unchanged ·")} {plan.skippedShared.length}  {tr("occupied proxies skipped")}</p>
          <div className="assign-preview">
            <table>
              <thead><tr><th>{tr("Profile")}</th><th>{tr("Current proxy")}</th><th>{tr("Assigned proxy")}</th></tr></thead>
              <tbody>{targets.map((p) => {
                const pair = plan.pairs.find((pair) => pair.profileId === p.id);
                return <tr key={p.id}><td>{p.name}</td><td>{proxyName(p.proxy_id)}</td><td>{pair ? proxyName(pair.proxyId) : tr("Unchanged")}</td></tr>;
              })}</tbody>
            </table>
          </div>
          {blocked && <p className="error">{tr("Stop the selected browsers before assigning proxies")}</p>}
          {error && <p className="error" role="alert">{error}</p>}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" disabled={busy} onClick={onClose}>{tr("Cancel")}</button>
          <button className="btn-primary" disabled={busy || blocked || plan.pairs.length === 0} onClick={apply}>{busy ? tr("Applying…") : tr("Apply assignment")}</button>
        </footer>
      </div>
    </DialogBackdrop>
  );
}

function QuickEditDialog({
  kind, profile, proxies, proxySnapshots, onClose, onSaved,
}: {
  kind: "proxy" | "notes";
  profile: ProfileMeta;
  proxies: ProxyEntry[];
  proxySnapshots: Record<string, ProxyTestSnapshot>;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [proxyId, setProxyId] = useState<string | null>(profile.proxy_id);
  const [notes, setNotes] = useState(profile.notes);

  const saveProxy = async () => {
    try {
      await invoke("profile_bind_proxy", { profileId: profile.id, proxyId });
      toast.ok(tr("Proxy updated"));
      onSaved();
    } catch (e) { toast.err(String(e)); }
  };

  const saveNotes = async () => {
    try {
      // Round-trip the whole profile JSON so the user's other fields stay intact.
      const stored = await invoke<any>("profile_get", { id: profile.id });
      stored.notes = notes;
      await invoke<ProfileMeta>("profile_save", { payload: stored });
      toast.ok(tr("Notes saved"));
      onSaved();
    } catch (e) { toast.err(String(e)); }
  };

  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog">
        <header className="dialog-head">
          <h2>{kind === "proxy" ? tr("Bind proxy") : tr("Edit notes")} — {profile.name}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          {kind === "proxy" ? (
            <label>
              <span className="lbl">{tr("Proxy")}</span>
              <select value={proxyId ?? ""} onChange={(e) => setProxyId(e.target.value || null)}>
                <option value="">{tr("— direct connection —")}</option>
                {proxies.map((px) => (
                  <option key={px.id} value={px.id}>
                    {proxyBindingLabel(px, proxySnapshots[px.id])}
                  </option>
                ))}
              </select>
            </label>
          ) : (
            <label>
              <span className="lbl">{tr("Notes")}</span>
              <textarea rows={6} value={notes} onChange={(e) => setNotes(e.target.value)} autoFocus />
            </label>
          )}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>{tr("Cancel")}</button>
          <button className="btn-primary" onClick={kind === "proxy" ? saveProxy : saveNotes}>
            <Save /> {tr("Save")} </button>
        </footer>
      </div>
    </DialogBackdrop>
  );
}

// ---- inline editor ----

type OsPlatform = "macOS" | "Windows" | "Linux";
const OS_OPTIONS: { id: OsPlatform; label: string }[] = [
  { id: "macOS",   label: "macOS"   },
  { id: "Windows", label: "Windows" },
  { id: "Linux",   label: "Linux"   },
];

function InlineEditor({
  draft, nameError, setDraft, proxies, fingerprints, onSave, onCancel, busy,
}: {
  draft: ProfileForm;
  nameError: string | null;
  setDraft: (f: ProfileForm) => void;
  proxies: ProxyEntry[];
  fingerprints: FingerprintEntry[];
  onSave: () => void;
  onCancel: () => void;
  busy: boolean;
}) {
  const f = draft;
  const u = <K extends keyof ProfileForm>(k: K, v: ProfileForm[K]) => setDraft({ ...f, [k]: v });
  const draftRef = useRef(f);
  draftRef.current = f;
  const gpuPickRequest = useRef(0);
  const gpuRecommendationRequest = useRef(0);
  const [hardwareSource, setHardwareSource] = useState<{
    presetId: string;
    configs: HardwareConfig[];
  } | null>(null);

  // OS filter init from bound fingerprint's platform; new profile uses host OS.
  const currentFp = fingerprints.find((x) => x.id === f.gpu_preset_id);
  const [osFilter, setOsFilter] = useState<OsPlatform>(
    (currentFp?.platform as OsPlatform) ?? HOST_OS
  );
  const gpusForOs = useMemo(
    () => fingerprints.filter((fp) => fp.platform === osFilter),
    [fingerprints, osFilter],
  );

  /// Pick GPU = full fingerprint snap; toStored carries lib.payload at save.
  const setGpu = async (id: string, fromRecommendation = false) => {
    if (!fromRecommendation) gpuRecommendationRequest.current += 1;
    const fp = fingerprints.find((x) => x.id === id);
    if (!fp) return;
    const nav = fp.payload?.navigator ?? {};
    const request = ++gpuPickRequest.current;
    let picks: PresetEnrichPicks;
    try {
      picks = await invoke<PresetEnrichPicks>("enrich_picks_for_preset", { presetId: id });
    } catch (e) {
      toast.err(tr("Unable to load hardware configurations: {0}", String(e)));
      return;
    }
    if (request !== gpuPickRequest.current) return;
    const configs = picks.hardware_configs ?? [];
    const selectedIsValid = configs.some(
      (c) =>
        c.hardware_concurrency === picks.hardware_concurrency &&
        c.device_memory === picks.device_memory,
    );
    if (!selectedIsValid) {
      toast.err(tr("Backend returned an invalid hardware configuration"));
      return;
    }
    setHardwareSource({ presetId: id, configs });
    const current = draftRef.current;
    setDraft({
      ...current,
      gpu_preset_id: id,
      hardware_concurrency: picks.hardware_concurrency,
      device_memory: picks.device_memory,
      platform_version: picks.platform_version ?? nav.platform_version ?? current.platform_version,
      user_agent: nav.user_agent ?? current.user_agent,
    });
  };

  const setRecommendedGpu = async (os: OsPlatform, replaceCurrent: boolean) => {
    const request = ++gpuRecommendationRequest.current;
    const pool = fingerprints.filter((fp) => fp.platform === os);
    const fallbackPool = pool.length > 0 ? pool : fingerprints;
    try {
      const id = await invoke<string>("fingerprint_recommend", { platform: os });
      if (request !== gpuRecommendationRequest.current) return;
      const currentId = draftRef.current.gpu_preset_id;
      const currentExists = fingerprints.some((fp) => fp.id === currentId);
      if (!replaceCurrent && currentExists) return;
      if (fingerprints.some((fp) => fp.id === id)) {
        await setGpu(id, true);
        return;
      }
      console.warn(tr("recommended fingerprint is not in the loaded library: {0}", id));
    } catch (e) {
      if (request !== gpuRecommendationRequest.current) return;
      console.warn("fingerprint recommendation failed:", e);
    }

    const fallback = fallbackPool[Math.floor(Math.random() * fallbackPool.length)];
    if (fallback && request === gpuRecommendationRequest.current) {
      await setGpu(fallback.id, true);
    }
  };

  // Existing profiles did not pass through setGpu in this editor session.
  // Load the same backend-owned combinations without changing their values.
  useEffect(() => {
    const presetId = f.gpu_preset_id;
    if (!presetId || hardwareSource?.presetId === presetId) return;
    let disposed = false;
    invoke<PresetEnrichPicks>("enrich_picks_for_preset", { presetId })
      .then((picks) => {
        if (!disposed) {
          setHardwareSource({ presetId, configs: picks.hardware_configs ?? [] });
        }
      })
      .catch((e) => {
        if (!disposed) console.warn("hardware configuration load failed:", e);
      });
    return () => { disposed = true; };
  }, [f.gpu_preset_id, hardwareSource?.presetId]);

  // Pick an unused or least-used template only for a fresh identity. An
  // existing legacy profile with no match must keep its stored fingerprint.
  useEffect(() => {
    if (fingerprints.length === 0 || f.id) return;
    const exists = fingerprints.some((g) => g.id === f.gpu_preset_id);
    if (!exists) {
      void setRecommendedGpu(osFilter, false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fingerprints, osFilter, f.gpu_preset_id]);

  const pickOs = (os: OsPlatform) => {
    setOsFilter(os);
    // An OS change is an automatic choice, so prefer its least-used template.
    if (!currentFp || currentFp.platform !== os) {
      void setRecommendedGpu(os, true);
    }
  };

  const hardwareConfigs =
    hardwareSource?.presetId === f.gpu_preset_id ? hardwareSource.configs : [];
  const cpuOptions = [...new Set(hardwareConfigs.map((c) => c.hardware_concurrency))]
    .sort((a, b) => a - b);
  if (!cpuOptions.includes(f.hardware_concurrency)) cpuOptions.push(f.hardware_concurrency);
  const memoryOptions = [...new Set(
    hardwareConfigs
      .filter((c) => c.hardware_concurrency === f.hardware_concurrency)
      .map((c) => c.device_memory),
  )].sort((a, b) => a - b);
  if (!memoryOptions.includes(f.device_memory)) memoryOptions.push(f.device_memory);

  const setCpu = (hardwareConcurrency: number) => {
    const allowedMemory = hardwareConfigs
      .filter((c) => c.hardware_concurrency === hardwareConcurrency)
      .map((c) => c.device_memory);
    const deviceMemory = allowedMemory.includes(f.device_memory)
      ? f.device_memory
      : allowedMemory[0] ?? f.device_memory;
    setDraft({
      ...f,
      hardware_concurrency: hardwareConcurrency,
      device_memory: deviceMemory,
    });
  };

  const setMemory = (deviceMemory: number) => {
    const valid = hardwareConfigs.length === 0 || hardwareConfigs.some(
      (c) =>
        c.hardware_concurrency === f.hardware_concurrency &&
        c.device_memory === deviceMemory,
    );
    if (valid) u("device_memory", deviceMemory);
  };

  return (
    <fieldset className="inline-editor" disabled={busy}>
      <div className="ie-stripe" />
      <div className="ie-grid">
        {/* ----- col 1: identity + hardware ----- */}
        <div className="ie-section">
          <div className="ie-section-title">{tr("Identity")}</div>
          <Field
            label={tr("Profile name")}
            value={f.name}
            onChange={(v) => u("name", v)}
            placeholder={tr("e.g. shop-pl-1")}
            error={nameError}
          />

          <label>
            <span className="lbl">{tr("Operating system")}</span>
            <div className="seg">
              {OS_OPTIONS.map((o) => (
                <button
                  key={o.id}
                  type="button"
                  className={`seg-btn ${osFilter === o.id ? "active" : ""}`}
                  onClick={() => pickOs(o.id)}
                >
                  {o.label}
                </button>
              ))}
            </div>
          </label>

          <label>
            <span className="lbl">{tr("GPU / device (from Fingerprint Library)")}</span>
            <CSSelect
              value={f.gpu_preset_id}
              onChange={(v) => setGpu(v)}
              placeholder={tr("— no {0} fingerprints in library —", osFilter)}
              options={gpusForOs.map((g) => ({ value: g.id, label: g.label }))}
            />
          </label>

          <Field label={tr("User-Agent")} value={f.user_agent} onChange={(v) => u("user_agent", v)} mono />

          <div className="form-row">
            <SelectField
              label={tr("Logical CPUs")}
              value={f.hardware_concurrency}
              onChange={setCpu}
              options={cpuOptions}
            />
            <SelectField
              label={tr("Memory (GB)")}
              value={f.device_memory}
              onChange={setMemory}
              options={memoryOptions}
            />
          </div>

          <label>
            <span className="lbl">{tr("Proxy")}</span>
            <CSSelect
              value={f.proxy_id ?? ""}
              onChange={(v) => u("proxy_id", v ? v : null)}
              options={[
                { value: "", label: tr("— direct connection —") },
                ...proxies.map((px) => ({
                  value: px.id,
                  label: `${px.name || `${px.host}:${px.port}`} · ${px.country || px.kind}`,
                })),
              ]}
            />
          </label>
        </div>

        {/* ----- col 2: locale + noise ----- */}
        <div className="ie-section">
          <div className="ie-section-title">{tr("Locale")}</div>
          <div className="form-row">
            <label>
              <span className="lbl">{tr("Timezone")}</span>
              <CSSelect
                value={f.timezone}
                onChange={(v) => u("timezone", v)}
                options={TIMEZONES.map((tz) => ({
                  value: tz,
                  label: tz === AUTO_TZ ? tr("Auto (from proxy geo)") : tz,
                }))}
              />
            </label>
            <label>
              <span className="lbl">{tr("Language")}</span>
              <CSSelect
                value={f.language}
                onChange={(v) => u("language", v)}
                options={LOCALES.map((l) => ({ value: l.code, label: tr(l.label) }))}
              />
            </label>
          </div>

          <div className="ie-section-title" style={{ marginTop: 6 }}>{tr("Noise")}</div>
          <div className="noise-grid noise-grid-3">
            <Pair label="Canvas"        value={f.noise_canvas}        on={(v) => u("noise_canvas", v)} />
            <Pair label="WebGL"         value={f.noise_webgl}         on={(v) => u("noise_webgl", v)} />
            <Pair label={tr("Audio")}         value={f.noise_audio}         on={(v) => u("noise_audio", v)} />
            <Pair label={tr("Client rects")}  value={f.noise_client_rects}  on={(v) => u("noise_client_rects", v)} />
            <Pair label={tr("Sensors")}       value={f.noise_sensors}       on={(v) => u("noise_sensors", v)} />
            <Pair label={tr("Fonts")}         value={f.noise_fonts}         on={(v) => u("noise_fonts", v)} />
          </div>

          <PortList
            label={tr("Ports to block")}
            value={f.blocked_ports}
            onChange={(v) => u("blocked_ports", v)}
          />
        </div>

        {/* ----- col 3: privacy + media + notes ----- */}
        <div className="ie-section">
          <div className="ie-section-title">{tr("Privacy")}</div>
          <div className="form-row">
            <label>
              <span className="lbl">WebRTC</span>
              <CSSelect
                value={f.webrtc}
                onChange={(v) => u("webrtc", v as WebRtcMode)}
                options={[
                  { value: "auto", label: tr("Auto") },
                  { value: "tcp_only", label: tr("TCP only") },
                  { value: "block", label: tr("Block") },
                ]}
              />
            </label>
            <label>
              <span className="lbl">{tr("Do Not Track")}</span>
              <CSSelect
                value={f.do_not_track ? "1" : "0"}
                onChange={(v) => u("do_not_track", v === "1")}
                options={[
                  { value: "0", label: tr("Off") },
                  { value: "1", label: tr("On (send DNT: 1)") },
                ]}
              />
            </label>
          </div>

          <label>
            <span className="lbl">{tr("Geolocation")}</span>
            <div className="seg seg-2">
              {(["auto", "manual"] as GeoMode[]).map((m) => (
                <button key={m} className={`seg-btn ${f.geo_mode === m ? "active" : ""}`} onClick={() => u("geo_mode", m)}>
                  {m === "auto" ? tr("Auto (from proxy)") : tr("Manual coords")}
                </button>
              ))}
            </div>
          </label>
          {f.geo_mode === "manual" && (
            <div className="form-row form-row-3">
              <NumField label={tr("Latitude")} value={f.geo_lat} onChange={(v) => u("geo_lat", v)} step={0.0001} />
              <NumField label={tr("Longitude")} value={f.geo_lng} onChange={(v) => u("geo_lng", v)} step={0.0001} />
              <NumField label={tr("Accuracy m")} value={f.geo_accuracy} onChange={(v) => u("geo_accuracy", v)} />
            </div>
          )}

          <div className="ie-section-title" style={{ marginTop: 10 }}>{tr("Media devices")}</div>
          <div className="form-row form-row-3">
            <SelectField label={tr("Mic in")} value={f.media_audio_in} onChange={(v) => u("media_audio_in", v)} options={MEDIA_COUNT_OPTIONS} />
            <SelectField label={tr("Speakers")} value={f.media_audio_out} onChange={(v) => u("media_audio_out", v)} options={MEDIA_COUNT_OPTIONS} />
            <SelectField label={tr("Webcam")} value={f.media_video_in} onChange={(v) => u("media_video_in", v)} options={MEDIA_COUNT_OPTIONS} />
          </div>

          <label>
            <span className="lbl">{tr("Notes")}</span>
            <textarea rows={2} value={f.notes} onChange={(e) => u("notes", e.target.value)} placeholder={tr("Free-form notes…")} />
          </label>
        </div>
      </div>
      <ExtensionPicker value={f.extensions} onChange={(ids) => u("extensions", ids)} />
      <div className="ie-foot">
        <button className="btn-ghost" onClick={onCancel}>{tr("Cancel")}</button>
        <button className="btn-primary" onClick={onSave} disabled={nameError != null} title={nameError ?? undefined}>
          {f.id ? <Save /> : <Plus />} {busy ? tr("Saving…") : f.id ? tr("Save changes") : tr("Create profile")}
        </button>
      </div>
    </fieldset>
  );
}

function ShardMini() {
  return <svg width="12" height="12" viewBox="0 0 12 12"><path d="M6 1L11 6L6 11L1 6Z" fill="currentColor" /></svg>;
}

// ---- shared inputs ----

type FieldProps = {
  label: string;
  value: string;
  onChange: (v: string) => void;
  type?: string;
  placeholder?: string;
  mono?: boolean;
  error?: string | null;
};
function Field({ label, value, onChange, type = "text", placeholder, mono, error }: FieldProps) {
  const className = [mono ? "mono" : "", error ? "input-invalid" : ""].filter(Boolean).join(" ");
  return (
    <label>
      <span className="lbl">{label}</span>
      <input
        className={className}
        type={type}
        value={value}
        placeholder={placeholder}
        aria-invalid={error ? true : undefined}
        onChange={(e) => onChange(e.target.value)}
      />
      {error && <span className="field-error">{error}</span>}
    </label>
  );
}

function NumField({ label, value, onChange, step }: { label: string; value: number; onChange: (v: number) => void; step?: number }) {
  return (
    <label>
      <span className="lbl">{label}</span>
      <input type="number" step={step ?? 1} value={value} onChange={(e) => onChange(parseFloat(e.target.value) || 0)} />
    </label>
  );
}

function Pair({
  label, value, on, blockLabel,
}: {
  label: string;
  value: NoiseMode;
  on: (v: NoiseMode) => void;
  /// Allow/Block labels instead of Real/Auto (used by Ports).
  blockLabel?: boolean;
}) {
  const opts: NoiseMode[] = ["real", "auto"];
  const labelFor = (o: NoiseMode) =>
    blockLabel
      ? (o === "real" ? tr("Allow") : tr("Block"))
      : (o === "real" ? tr("Real") : tr("Auto noise"));
  return (
    <label>
      <span className="lbl">{label}</span>
      <div className="tri tri-2">
        {opts.map((o) => (
          <button key={o} className={`tri-btn ${value === o ? "active" : ""}`} onClick={() => on(o)}>
            {labelFor(o)}
          </button>
        ))}
      </div>
    </label>
  );
}

function PortList({
  label, value, onChange,
}: {
  label: string;
  value: number[];
  onChange: (v: number[]) => void;
}) {
  const [text, setText] = useState("");
  const commit = () => {
    // Accept "3389", "3389, 5900", "3389 5900"; drops non-numeric tokens.
    const toks = text.split(/[\s,]+/).filter(Boolean);
    if (toks.length === 0) return;
    const next = new Set(value);
    for (const t of toks) {
      const n = parseInt(t, 10);
      if (Number.isFinite(n) && n >= 1 && n <= 65535) next.add(n);
    }
    onChange([...next].sort((a, b) => a - b));
    setText("");
  };
  const remove = (p: number) => onChange(value.filter((x) => x !== p));
  return (
    <label className="port-list-wrap">
      <span className="lbl">{label}</span>
      <div className="port-list">
        {value.map((p) => (
          <span key={p} className="port-chip">
            <span>{p}</span>
            <button type="button" className="port-chip-x" onClick={() => remove(p)} title={tr("Remove")} aria-label={`${tr("Remove")} ${p}`}><X size={14} /></button>
          </span>
        ))}
        <input
          type="text"
          inputMode="numeric"
          className="port-input"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter" || e.key === "," || e.key === " ") { e.preventDefault(); commit(); } }}
          onBlur={commit}
          placeholder={value.length === 0 ? tr("e.g. 3389, 5900, 8080") : tr("add port…")}
        />
      </div>
    </label>
  );
}

type CSOption<T> = { value: T; label: string };

/// Themed dropdown replacing the native select inside the profile editor.
function CSSelect<T extends string | number>({
  value, options, onChange, placeholder,
}: {
  value: T;
  options: CSOption<T>[];
  onChange: (v: T) => void;
  placeholder?: string;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  // Body portal so ancestor overflow:hidden can't clip the menu.
  const [anchor, setAnchor] = useState<{ left: number; top: number; width: number; up: boolean } | null>(null);

  const place = () => {
    const el = triggerRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const menuH = Math.min(280, options.length * 38 + 10);
    // Flip up if no room below.
    const up = r.bottom + menuH + 8 > window.innerHeight && r.top - menuH - 8 > 0;
    setAnchor({
      left: r.left,
      top: up ? r.top - menuH - 4 : r.bottom + 4,
      width: r.width,
      up,
    });
  };

  const toggle = () => {
    if (!open) place();
    setOpen((v) => !v);
  };

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node;
      if (triggerRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    // Re-anchor on page scroll/resize; ignore scrolls inside the menu.
    const onScroll = (e: Event) => {
      if (menuRef.current && menuRef.current.contains(e.target as Node)) return;
      place();
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    window.addEventListener("resize", place);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [open]);

  const current = options.find((o) => o.value === value);
  return (
    <div className={`cs ${open ? "cs-open" : ""}`}>
      <button ref={triggerRef} type="button" className="cs-trigger" onClick={toggle}>
        <span className="cs-value">{current?.label ?? placeholder ?? ""}</span>
        <span className="cs-caret" aria-hidden>▾</span>
      </button>
      {open && anchor && createPortal(
        <div
          ref={menuRef}
          className="cs-menu"
          role="listbox"
          style={{ left: anchor.left, top: anchor.top, width: anchor.width }}
        >
          {options.map((o) => (
            <div
              key={String(o.value)}
              role="option"
              aria-selected={o.value === value}
              className={`cs-opt ${o.value === value ? "active" : ""}`}
              onClick={() => { onChange(o.value); setOpen(false); }}
            >
              {o.label}
            </div>
          ))}
        </div>,
        document.body,
      )}
    </div>
  );
}

function SelectField<T extends string | number>({
  label, value, onChange, options, format,
}: {
  label: string;
  value: T;
  onChange: (v: T) => void;
  options: readonly T[];
  format?: (v: T) => string;
}) {
  const opts: CSOption<T>[] = options.map((o) => ({
    value: o,
    label: format ? format(o) : String(o),
  }));
  return (
    <label>
      <span className="lbl">{label}</span>
      <CSSelect value={value} options={opts} onChange={onChange} />
    </label>
  );
}

function Metric({ label, value, accent, running }: { label: string; value: string; accent?: boolean; running?: boolean }) {
  return (
    <div className={`metric ${accent ? "metric-accent" : ""}`}>
      <dt className="m-k">{label}</dt>
      <dd className={`m-v ${running ? "m-v-running" : ""}`}>{value}</dd>
    </div>
  );
}

// ---- Proxies ----

type ProxyTestSnapshot = {
  first_seen: string;
  last_seen: string;
  ip: string;
  country_code: string;
  country: string;
  region: string;
  city: string;
  isp: string;
  timezone: string;
  latitude: number;
  longitude: number;
  tcp_ms: number | null;
  udp_ms: number | null;
  udp_error: string | null;
  provider: string;
};

type ProxyBatchTestResult = {
  index: number;
  snapshot: ProxyTestSnapshot | null;
  error: string | null;
};

function proxyTestFailureReason(snapshot: ProxyTestSnapshot): string | null {
  if (snapshot.tcp_ms == null) return tr("Proxy TCP test failed");
  if (!snapshot.ip.trim()) return tr("Proxy test did not return an exit IP");
  return null;
}

function proxyTestIsActive(snapshot: ProxyTestSnapshot): boolean {
  return proxyTestFailureReason(snapshot) == null;
}

/// Dispatch one command per proxy so completed rows can render immediately.
/// The backend's shared semaphore still limits all manual tests to five in
/// flight, including single-row tests started elsewhere in the UI.
async function testProxiesIncrementally(
  entries: ProxyEntry[],
  onResult: (result: ProxyBatchTestResult) => void,
): Promise<ProxyBatchTestResult[]> {
  const results = new Array<ProxyBatchTestResult>(entries.length);
  let nextIndex = 0;
  const worker = async () => {
    while (nextIndex < entries.length) {
      const index = nextIndex;
      nextIndex += 1;
      const entry = entries[index];
      let result: ProxyBatchTestResult;
      try {
        const snapshot = await invoke<ProxyTestSnapshot>("proxy_full_test", { entry });
        result = { index, snapshot, error: null };
      } catch (error) {
        result = { index, snapshot: null, error: String(error) };
      }
      results[index] = result;
      onResult(result);
    }
  };
  const workerCount = Math.min(entries.length, 5);
  await Promise.all(Array.from({ length: workerCount }, worker));
  return results;
}

function ProxiesView() {
  const [proxies, setProxies] = useState<ProxyEntry[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [editing, setEditing] = useState<ProxyEntry | null>(null);
  const [bulkOpen, setBulkOpen] = useState(false);
  const [snapshots, setSnapshots] = useState<Record<string, ProxyTestSnapshot>>({});
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [infoFor, setInfoFor] = useState<{ proxy: ProxyEntry; anchor: { x: number; y: number } } | null>(null);
  const [proxySel, setProxySel] = useState<Set<string>>(new Set());
  const [renaming, setRenaming] = useState<{ id: string; draft: string } | null>(null);
  const [profiles, setProfiles] = useState<ProfileMeta[]>([]);
  const [lockedProxyIds, setLockedProxyIds] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState("");
  const sort = useListSort();
  const ctx = useContextMenu();

  // Search filter: matches name / host / port / country tag / notes / username
  // *and* the exit IP from the latest snapshot (so the user can find a proxy
  // by "last seen exiting at X.X.X.X").  Whitespace-trimmed, case-insensitive.
  const filteredProxies = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return proxies;
    return proxies.filter((p) => {
      const ip = (snapshots[p.id]?.ip ?? "").toLowerCase();
      const city = (snapshots[p.id]?.city ?? "").toLowerCase();
      const isp = (snapshots[p.id]?.isp ?? "").toLowerCase();
      return (
        p.name.toLowerCase().includes(q) ||
        p.host.toLowerCase().includes(q) ||
        String(p.port).includes(q) ||
        p.country.toLowerCase().includes(q) ||
        p.notes.toLowerCase().includes(q) ||
        (p.username ?? "").toLowerCase().includes(q) ||
        ip.includes(q) ||
        city.includes(q) ||
        isp.includes(q)
      );
    });
  }, [proxies, snapshots, search]);

  const selectedHasLockedProxy = [...proxySel].some((id) => lockedProxyIds.has(id));

  // If a browser starts while a proxy editor or inline rename is open, close
  // the stale editor immediately. The backend lock remains authoritative
  // during the two-second process polling interval.
  useEffect(() => {
    if (editing && lockedProxyIds.has(editing.id)) setEditing(null);
    if (renaming && lockedProxyIds.has(renaming.id)) setRenaming(null);
  }, [lockedProxyIds, editing, renaming]);

  const commitRename = async () => {
    if (!renaming) return;
    const entry = proxies.find((p) => p.id === renaming.id);
    if (!entry) { setRenaming(null); return; }
    if (lockedProxyIds.has(entry.id)) {
      setRenaming(null);
      toast.err(tr("Stop the running browser using this proxy before renaming it"));
      return;
    }
    const newName = renaming.draft;
    if (newName === entry.name) { setRenaming(null); return; }
    const error = displayNameError(newName, entry.name, true);
    if (error) { toast.err(error); return; }
    try {
      const request: ProxySaveRequest = {
        id: entry.id,
        name: newName,
        kind: entry.kind,
        host: entry.host,
        port: entry.port,
        country: entry.country,
        notes: entry.notes,
        credentials: { mode: "keep" },
      };
      await invoke("proxy_save", { entry: request });
      setRenaming(null);
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const reload = async () => {
    try {
      setProxies(await invoke<ProxyEntry[]>("proxy_list"));
      // Profile list powers the per-proxy bound-count column.
      setProfiles(await invoke<ProfileMeta[]>("profile_list"));
      setLoadError(null);
    } catch (e) { setLoadError(String(e)); }
    finally { setLoading(false); }
  };
  useEffect(() => { reload(); }, []);
  // Pick up proxies/profiles added via the automation API or MCP live.
  useStoreChanged(reload);
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const locked = await invoke<string[]>("proxy_active_ids");
        if (!cancelled) setLockedProxyIds(new Set(locked));
      } catch {}
    };
    tick();
    const handle = setInterval(tick, 2000);
    return () => { cancelled = true; clearInterval(handle); };
  }, []);

  // proxy_id → bound-profile count (O(n) tally; n is small).
  const profileCountByProxy = useMemo(() => {
    const out: Record<string, number> = {};
    for (const p of profiles) {
      if (p.proxy_id) out[p.proxy_id] = (out[p.proxy_id] ?? 0) + 1;
    }
    return out;
  }, [profiles]);

  // Fetch latest snapshot per proxy so rows survive a launcher restart.
  useEffect(() => {
    if (proxies.length === 0) return;
    let cancelled = false;
    (async () => {
      const entries = await Promise.all(
        proxies.map(async (p) => {
          try {
            const snap = await invoke<ProxyTestSnapshot | null>("proxy_last_test", { id: p.id });
            return [p.id, snap] as const;
          } catch {
            return [p.id, null] as const;
          }
        }),
      );
      if (cancelled) return;
      const next: Record<string, ProxyTestSnapshot> = {};
      for (const [id, snap] of entries) if (snap) next[id] = snap;
      setSnapshots(next);
    })();
    return () => { cancelled = true; };
  }, [proxies]);

  const fullTest = async (p: ProxyEntry) => {
    setBusy((s) => ({ ...s, [p.id]: true }));
    try {
      const snap = await invoke<ProxyTestSnapshot>("proxy_full_test", { entry: p });
      setSnapshots((s) => ({ ...s, [p.id]: snap }));
      // Refresh: backend may have just populated the country tag.
      reload();
    } catch (e) {
      toast.err(`${p.name || p.host}: ${e}`);
    } finally {
      setBusy((s) => ({ ...s, [p.id]: false }));
    }
  };

  const remove = async (id: string) => {
    if (lockedProxyIds.has(id)) {
      toast.err(tr("Stop the running browser using this proxy before deleting it"));
      return;
    }
    if ((await confirmModal({ title: tr("Delete proxy"), message: tr("Delete this proxy?"), danger: true })) !== true) return;
    try { await invoke("proxy_delete", { id }); reload(); toast.ok(tr("Proxy deleted")); }
    catch (e) { toast.err(String(e)); }
  };

  // Each proxy reports independently while the backend keeps at most five
  // tests in flight. A slow timeout no longer withholds completed row results.
  const bulkTest = async () => {
    const ids = [...proxySel];
    if (ids.length === 0) return;
    const targets = proxies.filter((p) => proxySel.has(p.id));
    toast.info(tr("Testing {0} prox{1}…", ids.length, ids.length === 1 ? "y" : "ies"));
    setBusy((state) => {
      const next = { ...state };
      for (const target of targets) next[target.id] = true;
      return next;
    });
    try {
      const results = await testProxiesIncrementally(targets, (result) => {
        const target = targets[result.index];
        if (!target) return;
        const snapshot = result.snapshot;
        if (snapshot) {
          setSnapshots((state) => ({ ...state, [target.id]: snapshot }));
        }
        setBusy((state) => ({ ...state, [target.id]: false }));
      });
      let passed = 0;
      let failed = 0;
      let incomplete = 0;
      for (const result of results) {
        if (!result.snapshot) {
          incomplete += 1;
          continue;
        }
        if (proxyTestIsActive(result.snapshot)) passed += 1;
        else failed += 1;
      }
      await reload();
      const summary = tr("Bulk test done: {0} passed, {1} failed{2}", passed, failed, incomplete > 0 ? `, ${incomplete} incomplete` : "");
      if (failed === 0 && incomplete === 0) toast.ok(summary);
      else toast.info(summary);
    } catch (e) {
      toast.err(tr("Bulk test failed: {0}", e));
    } finally {
      setBusy((state) => {
        const next = { ...state };
        for (const target of targets) next[target.id] = false;
        return next;
      });
    }
  };

  const bulkDelete = async () => {
    const ids = [...proxySel];
    if (ids.length === 0) return;
    const lockedIds = ids.filter((id) => lockedProxyIds.has(id));
    if (lockedIds.length > 0) {
      toast.err(tr("Stop browsers using the {0} selected locked prox{1} before deleting", lockedIds.length, lockedIds.length === 1 ? "y" : "ies"));
      return;
    }
    if ((await confirmModal({ title: tr("Delete proxies"), message: tr("Delete {0} prox{1}?", ids.length, ids.length === 1 ? "y" : "ies"), danger: true })) !== true) return;
    for (const id of ids) {
      try { await invoke("proxy_delete", { id }); } catch (e) { toast.err(String(e)); }
    }
    setProxySel(new Set());
    reload();
    toast.ok(tr("Deleted {0}", ids.length));
  };

  // Export in bulk-import format so round-trip preserves country tag.
  const bulkExport = () => {
    const targets = proxies.filter((p) => proxySel.has(p.id));
    if (targets.length === 0) return;
    const lines = targets.map((p) => {
      const auth = p.username || p.password ? `${p.username ?? ""}:${p.password ?? ""}@` : "";
      const base = `${p.kind}://${auth}${p.host}:${p.port}`;
      const tag = p.country ? `  # country=${p.country}` : "";
      return base + tag;
    });
    const text = lines.join("\n");
    clip.write(text).then(
      () => toast.ok(tr("Copied {0} to clipboard", targets.length)),
      (e) => toast.err(tr("Copy failed: ") + String(e)),
    );
  };

  // Import from clipboard (one per line, bulkExport format).
  const bulkImportClipboard = async () => {
    try {
      const text = await clip.read();
      if (!text.trim()) { toast.err(tr("Clipboard is empty")); return; }
      const n = await invoke<number>("proxy_bulk_import", { text, kind: "socks5" });
      reload();
      toast.ok(tr("Imported {0} prox{1}", n, n === 1 ? "y" : "ies"));
    } catch (e) { toast.err(tr("Import failed: ") + String(e)); }
  };

  const handleProxyDragEnd = async (event: DragEndEvent) => {
    const drop = listDrop(event, filteredProxies, "proxy");
    if (!drop) return;
    const previous = proxies;
    setProxies(moveByAnchor(proxies, drop.id, drop.anchorId, drop.placement));
    try {
      await invoke("proxy_move_order", drop);
    } catch (error) {
      setProxies(previous);
      toast.err(tr("Could not save proxy order: {0}", String(error)));
    }
  };

  return (
    <DndContext
      {...sort}
      onDragEnd={handleProxyDragEnd}
    >
    <section className="page">
      <PageToolbar title={tr("Proxies")} search={{ value: search, onChange: setSearch }}>
        <button className="btn-ghost" onClick={bulkImportClipboard} title={tr("Import proxies from the clipboard")}><Icon.Download size={16} /> {tr("Import")}</button>
        <button className="btn-primary" onClick={() => setBulkOpen(true)}><Plus /> {tr("New proxy")}</button>
      </PageToolbar>
      <SelectionBar count={proxySel.size}>
        <button className="btn-ghost btn-sm" onClick={bulkTest}><Icon.Refresh size={16} /> {tr("Test")}</button>
        <button className="btn-ghost btn-sm" onClick={bulkExport}><Icon.Upload size={16} /> {tr("Export")}</button>
        <button
          className="btn-ghost btn-sm danger"
          onClick={bulkDelete}
          disabled={selectedHasLockedProxy}
          title={selectedHasLockedProxy ? tr("Stop browsers using selected proxies before deleting") : tr("Delete selected proxies")}
        ><Icon.Trash size={16} /> {tr("Delete")}</button>
      </SelectionBar>
      {loadError !== null && <ListState kind="error" text={tr("Unable to load proxies: {0}", loadError)} />}
      <div className="rows proxy-rows">
        <div className="rows-head p-cols">
          <div className="sort-head" title={tr("Drag rows to reorder")}><Icon.Grip size={16} /></div>
          <div>
            <input
              type="checkbox"
              title={tr("Select all matching rows")}
              checked={filteredProxies.length > 0 && filteredProxies.every((p) => proxySel.has(p.id))}
              ref={(el) => {
                if (!el) return;
                const any = filteredProxies.some((p) => proxySel.has(p.id));
                const all = filteredProxies.every((p) => proxySel.has(p.id));
                el.indeterminate = any && !all;
              }}
              onChange={(e) => {
                setProxySel((prev) => {
                  const next = new Set(prev);
                  if (e.target.checked) {
                    for (const p of filteredProxies) next.add(p.id);
                  } else {
                    for (const p of filteredProxies) next.delete(p.id);
                  }
                  return next;
                });
              }}
            />
          </div>
          <div>{tr("Name")}</div><div>{tr("Type")}</div><div>{tr("Host:Port")}</div><div>{tr("Country")}</div><div>{tr("Profiles")}</div><div>{tr("Test result")}</div><div className="head-actions">{tr("Actions")}</div>
        </div>
        <SortableContext items={filteredProxies.map((proxy) => proxy.id)} strategy={verticalListSortingStrategy}>
        {filteredProxies.map((p) => {
          const r = snapshots[p.id];
          const testFailure = r ? proxyTestFailureReason(r) : null;
          const isActive = !!r && testFailure == null;
          const isBusy = !!busy[p.id];
          const cc = r?.country_code || p.country || "";
          const isSel = proxySel.has(p.id);
          const isLocked = lockedProxyIds.has(p.id);
          return (
            <SortableRow
              key={p.id}
              id={p.id}
              kind="proxy"
              name={p.name}
              className="row-wrap"
              rowClassName="row p-cols proxy-data-row"
              disabledReason={isLocked ? tr("Stop the browser using this proxy before reordering") : search.trim() ? tr("Clear search to reorder proxies") : renaming?.id === p.id ? tr("Finish renaming before reordering") : undefined}
              onContextMenu={(e) =>
                ctx.open(e, [
                  { label: tr("Test (TCP/UDP/geo)"), onClick: () => fullTest(p) },
                  { label: tr("View details"), onClick: () => setInfoFor({ proxy: p, anchor: { x: e.clientX, y: e.clientY } }) },
                  {
                    label: tr("Edit"),
                    onClick: () => setEditing(p),
                    disabled: isLocked,
                    title: isLocked ? tr("Stop the browser using this proxy before editing") : tr("Edit proxy"),
                  },
                  { sep: true, label: "", onClick: () => {} },
                  {
                    label: tr("Delete"),
                    onClick: () => remove(p.id),
                    danger: true,
                    disabled: isLocked,
                    title: isLocked ? tr("Stop the browser using this proxy before deleting") : tr("Delete proxy"),
                  },
                ])
              }
            >
                <div>
                  <input
                    type="checkbox"
                    checked={isSel}
                    onChange={() => {
                      setProxySel((s) => {
                        const n = new Set(s);
                        if (n.has(p.id)) n.delete(p.id); else n.add(p.id);
                        return n;
                      });
                    }}
                  />
                </div>
                <div className="cell-name">
                  {renaming?.id === p.id ? (
                    <input
                      autoFocus
                      className="inline-rename"
                      value={renaming.draft}
                      onChange={(e) => setRenaming({ id: p.id, draft: e.target.value })}
                      onBlur={commitRename}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") commitRename();
                        else if (e.key === "Escape") setRenaming(null);
                      }}
                    />
                  ) : (
                    <span
                      className={isLocked ? "cell-locked" : "cell-click"}
                      onClick={isLocked ? undefined : () => setRenaming({ id: p.id, draft: p.name })}
                      title={isLocked ? tr("Stop the browser using this proxy before renaming") : tr("Click to rename")}
                    >
                      {p.name || "—"}
                    </span>
                  )}
                </div>
                <div><span className={`badge badge-${p.kind}`}>{p.kind}</span></div>
                <div className="cell-hostport">
                  <span
                    className={`mono ${isLocked ? "cell-locked" : "cell-click"}`}
                    onClick={isLocked ? undefined : () => setEditing(p)}
                    title={isLocked ? tr("Stop the browser using this proxy before editing") : tr("Edit proxy")}
                  >
                    {p.host}:{p.port}
                  </span>
                </div>
                <div className="proxy-location">
                  {cc ? (
                    <span className="proxy-country">
                      <CountryFlag cc={cc} />
                      <span className="flag">{cc}</span>
                    </span>
                  ) : <span className="muted">—</span>}
                  <ProxyLocationDetail city={r?.city} />
                </div>
                <div>
                  <span className="profile-count" title={tr("{0} profile(s) bound to this proxy", profileCountByProxy[p.id] ?? 0)}>
                    {profileCountByProxy[p.id] ?? 0}
                  </span>
                </div>
                <div className="proxy-test-cell">
                  {!r && !isBusy && <span className="muted small">{tr("not tested")}</span>}
                  {isBusy && <span className="muted small">{tr("testing…")}</span>}
                  {r && !isBusy && (
                    <div className="proxy-test">
                      <span
                        className={`status-pill ${isActive ? "status-active" : "status-failed status-unavailable"}`}
                        title={isActive ? `TCP ${r.tcp_ms} ms` : (testFailure ?? tr("Proxy test failed"))}
                      >
                        {isActive ? tr("Active") : tr("Failed")}
                      </span>
                      {/* UDP remains a proxy capability used by WebRTC policy
                          and diagnostics. QUIC is disabled independently for
                          every launcher-managed browser. */}
                      {r.udp_ms != null && p.kind === "socks5" && (
                        <button
                          type="button"
                          className="status-pill status-udp status-link"
                          title={tr("UDP relay works ({0} ms) — QUIC remains disabled by launcher policy. Click for docs.", r.udp_ms)}
                          onClick={() => { openUrl(UDP_DOCS_URL).catch(() => {}); }}
                        >
                          UDP
                        </button>
                      )}
                      {r.udp_ms == null && (
                        <button
                          type="button"
                          className="status-pill status-no-udp status-unavailable status-link"
                          title={tr("No UDP support — WebRTC cannot use a proxied UDP relay; QUIC remains disabled. Click for docs.")}
                          onClick={() => { openUrl(UDP_DOCS_URL).catch(() => {}); }}
                        >
                          UDP
                        </button>
                      )}
                      {isActive && r.ip && (
                        <span className="test-ip mono small" title={r.isp}>{r.ip}</span>
                      )}
                    </div>
                  )}
                </div>
                <div className="row-actions">
                  <button
                    className="icon-btn"
                    onClick={(e) => setInfoFor({ proxy: p, anchor: { x: e.clientX, y: e.clientY } })}
                    title={tr("Details + history")}
                  ><Icon.Info size={16} /></button>
                  <button className="icon-btn" onClick={() => fullTest(p)} disabled={isBusy} title={tr("Test TCP + UDP + geo")}><Icon.Refresh size={16} /></button>
                  <button
                    className="icon-btn"
                    onClick={() => setEditing(p)}
                    disabled={isLocked}
                    title={isLocked ? tr("Stop the browser using this proxy before editing") : tr("Edit")}
                  ><Icon.Edit size={16} /></button>
                  <button
                    className="icon-btn danger"
                    onClick={() => remove(p.id)}
                    disabled={isLocked}
                    title={isLocked ? tr("Stop the browser using this proxy before deleting") : tr("Delete")}
                  ><Icon.Trash size={16} /></button>
                </div>
            </SortableRow>
          );
        })}
        </SortableContext>
        {filteredProxies.length === 0 && loadError === null && (loading
          ? <ListState kind="loading" text={tr("Loading…")} />
          : <ListState kind="empty" text={tr(search.trim() ? "No matching proxies" : "No proxies yet")} />)}
      </div>
      {editing && <ProxyEditor initial={editing} onClose={() => { setEditing(null); reload(); }} />}
      {bulkOpen && <ProxyBulkImporter onClose={() => { setBulkOpen(false); reload(); }} />}
      {infoFor && (
        <ProxyInfoPopover
          proxy={infoFor.proxy}
          anchor={infoFor.anchor}
          latest={snapshots[infoFor.proxy.id]}
          onClose={() => setInfoFor(null)}
        />
      )}
      {ctx.node}
    </section>
    <ListSortOverlay />
    </DndContext>
  );
}

/// Proxy detail popover: latest IP/geo + UDP + IP-change history.
function ProxyInfoPopover({
  proxy, anchor, latest, onClose,
}: {
  proxy: ProxyEntry;
  anchor: { x: number; y: number };
  latest?: ProxyTestSnapshot;
  onClose: () => void;
}) {
  const [history, setHistory] = useState<ProxyTestSnapshot[]>([]);
  useEffect(() => {
    invoke<ProxyTestSnapshot[]>("proxy_history", { id: proxy.id })
      .then((h) => setHistory([...h].reverse()))
      .catch((e) => toast.err(String(e)));
  }, [proxy.id]);
  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (!t.closest(".proxy-popover")) onClose();
    };
    window.addEventListener("mousedown", onDoc);
    return () => window.removeEventListener("mousedown", onDoc);
  }, [onClose]);

  // Clamp inside viewport to avoid clipping at the right edge.
  const left = Math.min(anchor.x, window.innerWidth - 360);
  const top = Math.min(anchor.y + 8, window.innerHeight - 320);
  const latestFailure = latest ? proxyTestFailureReason(latest) : null;

  return (
    <div className="proxy-popover" style={{ left, top }} onClick={(e) => e.stopPropagation()}>
      <div className="popover-section">
        {latest ? (
          <>
            {latest.ip && (
              <>
                <div className="pop-row">
                  <span className="pop-ico"><Icon.Globe size={16} /></span>
                  <span className="mono">{latest.ip}</span>
                </div>
                <div className="pop-row">
                  <span className="pop-ico">{latest.country_code ? <CountryFlag cc={latest.country_code} height={14} /> : <Icon.Globe size={16} />}</span>
                  <span>{[latest.region, latest.city].filter(Boolean).join(", ") || latest.country || "—"}</span>
                </div>
              </>
            )}
            {latest.timezone && (
              <div className="pop-row">
                <span className="pop-ico"><Icon.Clock size={16} /></span>
                <span>{latest.timezone}</span>
              </div>
            )}
            {latest.isp && (
              <div className="pop-row">
                <span className="pop-ico"><Icon.Building size={16} /></span>
                <span className="muted small">{latest.isp}</span>
              </div>
            )}
            <div className="pop-row pop-row-split">
              <span className={`pop-pill ${latestFailure == null ? "ok" : "err"}`} title={latestFailure ?? undefined}>
                TCP {latestFailure == null ? `${latest.tcp_ms} ms` : "✗"}
              </span>
              {proxy.kind === "socks5" && (
                <span
                  className={`pop-pill ${latest.udp_ms != null ? "ok" : "err"}`}
                  title={latest.udp_error ?? undefined}
                >
                  UDP {latest.udp_ms != null ? `${latest.udp_ms} ms` : "✗"}
                </span>
              )}
            </div>
          </>
        ) : (
          <div className="muted small">{tr("Not tested yet — click ↻ on the row.")}</div>
        )}
      </div>
      <div className="popover-divider">{tr("IP HISTORY")}</div>
      <div className="popover-history">
        {history.length === 0 && <div className="muted small" style={{ padding: "10px 0" }}>{tr("No history yet")}</div>}
        {history.map((s, i) => (
          <div key={`${s.ip}-${s.first_seen}-${i}`} className="history-item">
            <div className="hi-head">
              <span className="mono">{s.ip || "—"}</span>
              {s.country_code && (
                <>
                  <CountryFlag cc={s.country_code} />
                  <span className="flag">{s.country_code}</span>
                </>
              )}
              {s.city && <span className="muted small">{s.city}</span>}
            </div>
            <div className="hi-meta muted small">
              {fmtTs(s.first_seen)}
              {s.first_seen !== s.last_seen && <> → {fmtTs(s.last_seen)}</>}
              {s.udp_ms != null && <> · UDP ✓</>}
              {s.udp_error && <> · UDP ✗</>}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

const countryFlagCodes = new Set(flagCodes.split(" "));

function CountryFlag({ cc, height = 15 }: { cc: string; height?: number }) {
  const code = cc.toLowerCase();
  if (!countryFlagCodes.has(code)) return null;
  return <img className="flag-rect" src={`${import.meta.env.BASE_URL}icons/flags/${code}.svg`} width={Math.round(height * 4 / 3)} height={height} alt="" />;
}

/// Format stored Unix timestamps in the launcher's locale and local time zone.
function fmtTs(stamp: string): string {
  if (!stamp.startsWith("@")) return stamp;
  const n = parseInt(stamp.slice(1), 10);
  if (!Number.isFinite(n)) return stamp;
  const d = new Date(n * 1000);
  return d.toLocaleString("zh-CN", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

type BulkRowState = {
  entry: ProxyEntry;
  selected: boolean;
  status: "idle" | "testing" | "ok" | "fail" | "incomplete";
  tcp_ms?: number | null;
  udp_ms?: number | null;
  country?: string;
  error?: string;
};

type ProxyBulkParseIssue = {
  line: number;
  reason: string;
};

type ProxyBulkParsePreview = {
  entries: ProxyEntry[];
  invalid: ProxyBulkParseIssue[];
  duplicate_lines: number;
  existing_duplicates: number;
};

/// Two-step bulk import: paste → validate/dedup preview → optional test → save selected.
function ProxyBulkImporter({ onClose }: { onClose: () => void }) {
  const [text, setText] = useState("");
  const [kind, setKind] = useState<ProxyEntry["kind"]>("socks5");
  const [rows, setRows] = useState<BulkRowState[]>([]);
  const [busy, setBusy] = useState(false);
  const [parseIssues, setParseIssues] = useState<ProxyBulkParseIssue[]>([]);
  const [skipped, setSkipped] = useState({ duplicateLines: 0, existingDuplicates: 0 });

  const parse = async () => {
    if (!text.trim()) { toast.err(tr("Nothing to parse")); return; }
    try {
      const preview = await invoke<ProxyBulkParsePreview>("proxy_bulk_parse", { text, kind });
      setParseIssues(preview.invalid);
      setSkipped({
        duplicateLines: preview.duplicate_lines,
        existingDuplicates: preview.existing_duplicates,
      });
      if (preview.invalid.length > 0) {
        toast.err(tr("Fix {0} invalid proxy line{1}", preview.invalid.length, preview.invalid.length === 1 ? "" : "s"));
        return;
      }
      const duplicateCount = preview.duplicate_lines + preview.existing_duplicates;
      if (preview.entries.length === 0) {
        toast.info(duplicateCount > 0 ? tr("No new proxies: every valid line is a duplicate") : tr("No valid proxy lines found"));
        return;
      }
      setRows(preview.entries.map((entry) => ({ entry, selected: true, status: "idle" })));
      if (duplicateCount > 0) {
        toast.info(tr("Skipped {0} duplicate proxy line{1}", duplicateCount, duplicateCount === 1 ? "" : "s"));
      }
    } catch (e) { toast.err(String(e)); }
  };

  const testOne = async (idx: number) => {
    setRows((rs) => rs.map((r, i) => i === idx ? { ...r, status: "testing" } : r));
    const entry = rows[idx]?.entry;
    if (!entry) return;
    try {
      const snap = await invoke<ProxyTestSnapshot>("proxy_full_test", { entry });
      const failure = proxyTestFailureReason(snap);
      setRows((rs) => rs.map((r, i) =>
        i === idx
          ? {
              ...r,
              status: failure == null ? "ok" : "fail",
              tcp_ms: snap.tcp_ms,
              udp_ms: snap.udp_ms,
              country: snap.country_code || r.country,
              error: failure ?? undefined,
              entry: { ...r.entry, country: snap.country_code || r.entry.country },
            }
          : r,
      ));
    } catch (e) {
      setRows((rs) => rs.map((r, i) => i === idx ? { ...r, status: "incomplete", error: String(e) } : r));
    }
  };

  const testAll = async () => {
    if (rows.length === 0) return;
    setBusy(true);
    setRows((current) => current.map((row) => ({ ...row, status: "testing", error: undefined })));
    try {
      await testProxiesIncrementally(rows.map((row) => row.entry), (result) => {
        setRows((current) => current.map((row, index) => {
          if (index !== result.index) return row;
          const snap = result.snapshot;
          if (!snap) {
            return {
              ...row,
              status: "incomplete",
              error: result.error ?? tr("Proxy test failed"),
            };
          }
          const failure = proxyTestFailureReason(snap);
          return {
            ...row,
            status: failure == null ? "ok" : "fail",
            tcp_ms: snap.tcp_ms,
            udp_ms: snap.udp_ms,
            country: snap.country_code || row.country,
            error: failure ?? result.error ?? undefined,
            entry: { ...row.entry, country: snap.country_code || row.entry.country },
          };
        }));
      });
    } catch (e) {
      setRows((current) => current.map((row) => row.status === "testing"
        ? { ...row, status: "incomplete", error: String(e) }
        : row));
    } finally {
      setBusy(false);
    }
  };

  const saveSelected = async () => {
    const entries = rows.filter((r) => r.selected).map((r) => r.entry);
    if (entries.length === 0) { toast.err(tr("Nothing selected")); return; }
    try {
      const n = await invoke<number>("proxy_bulk_save", { entries });
      const skippedOnSave = entries.length - n;
      if (skippedOnSave > 0) {
        toast.info(tr("Imported {0}; skipped {1} duplicate{2}", n, skippedOnSave, skippedOnSave === 1 ? "" : "s"));
      } else {
        toast.ok(tr("Imported {0} prox{1}", n, n === 1 ? "y" : "ies"));
      }
      onClose();
    } catch (e) { toast.err(String(e)); }
  };

  const allSel = rows.length > 0 && rows.every((r) => r.selected);
  const selCount = rows.filter((r) => r.selected).length;
  const skippedCount = skipped.duplicateLines + skipped.existingDuplicates;

  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog dialog-wide">
        <header className="dialog-head">
          <h2>{tr("Bulk import proxies")}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          {rows.length === 0 ? (
            <>
              <label>
                <span className="lbl">{tr("Default type (used when a line has no scheme)")}</span>
                <select value={kind} onChange={(e) => setKind(e.target.value as ProxyEntry["kind"])}>
                  <option value="socks5">SOCKS5</option>
                  <option value="http">HTTP</option>
                  <option value="https">HTTPS</option>
                </select>
              </label>
              <label>
                <span className="lbl">{tr("Paste one proxy per line")}</span>
                <textarea
                  rows={12}
                  className="mono"
                  value={text}
                  onChange={(e) => {
                    setText(e.target.value);
                    setParseIssues([]);
                    setSkipped({ duplicateLines: 0, existingDuplicates: 0 });
                  }}
                  placeholder={`socks5://user:pass@host:1080
user:pass@host:1080
host:1080:user:pass     # country=PL
host:8080               # no auth
# lines starting with # are ignored`}
                />
              </label>
              <p className="muted">
                 {tr("Invalid lines must be fixed. Duplicates use type, host, port, and username and are skipped automatically.")} </p>
              {parseIssues.length > 0 && (
                <div className="bulk-parse-errors" role="alert">
                  <strong>{tr("Invalid proxy format")}</strong>
                  <ul>
                    {parseIssues.slice(0, 6).map((issue) => (
                      <li key={`${issue.line}:${issue.reason}`}>{tr("Line")} {issue.line}: {issue.reason}</li>
                    ))}
                    {parseIssues.length > 6 && <li>{tr("And")} {parseIssues.length - 6}  {tr("more invalid lines")}</li>}
                  </ul>
                </div>
              )}
              {skippedCount > 0 && rows.length === 0 && (
                <div className="bulk-dedupe-note">
                  {tr("Found {0} duplicates: {1} repeated in this input, {2} already saved. They will not be imported.", skippedCount, skipped.duplicateLines, skipped.existingDuplicates)}
                </div>
              )}
            </>
          ) : (
            <>
              <div className="bulk-preview-head">
                <label className="bulk-preview-checkall">
                  <input
                    type="checkbox"
                    checked={allSel}
                    onChange={(e) =>
                      setRows((rs) => rs.map((r) => ({ ...r, selected: e.target.checked })))
                    }
                  />
                  <span>{selCount}  {tr("of")} {rows.length}  {tr("selected")}</span>
                </label>
                <div style={{ marginLeft: "auto", display: "flex", gap: 6 }}>
                  <button
                    className="btn-ghost btn-sm"
                    onClick={() => {
                      setRows([]);
                      setSkipped({ duplicateLines: 0, existingDuplicates: 0 });
                    }}
                  >{tr("← Back")}</button>
                  <button className="btn-ghost btn-sm" onClick={testAll} disabled={busy}>
                    {busy ? tr("Testing…") : <><Icon.Refresh size={16} />  {tr("Test all")}</>}
                  </button>
                  <button
                    className="btn-ghost btn-sm"
                    onClick={() =>
                      setRows((rs) =>
                        rs.map((r) => ({ ...r, selected: r.status === "ok" }))
                      )
                    }
                    title={tr("Tick only proxies whose latest test succeeded")}
                  >
                     {tr("✓ Keep working only")} </button>
                </div>
              </div>
              {skippedCount > 0 && (
                <div className="bulk-dedupe-note">
                  {tr("Skipped {0} duplicates", skippedCount)}
                  {skipped.existingDuplicates > 0 ? tr(" ({0} already saved)", skipped.existingDuplicates) : ""}.
                </div>
              )}
              <div className="bulk-preview-list">
                {rows.map((r, i) => (
                  <div key={`${r.entry.host}:${r.entry.port}:${i}`} className={`bulk-row bulk-row-${r.status}`}>
                    <input
                      type="checkbox"
                      checked={r.selected}
                      onChange={() =>
                        setRows((rs) => rs.map((x, j) => j === i ? { ...x, selected: !x.selected } : x))
                      }
                    />
                    <span className={`badge badge-${r.entry.kind}`}>{r.entry.kind}</span>
                    <span className="mono small bulk-host" title={`${r.entry.host}:${r.entry.port}${r.entry.username ? " @" + r.entry.username : ""}`}>
                      {r.entry.host}:{r.entry.port}
                      {r.entry.username && <span className="muted"> · {r.entry.username}</span>}
                    </span>
                    <div className="bulk-status">
                      {r.status === "idle" && <span className="muted small">{tr("not tested")}</span>}
                      {r.status === "testing" && <span className="muted small">{tr("testing…")}</span>}
                      {r.status === "ok" && (
                        <>
                          <span className="status-pill status-active" title={`TCP ${r.tcp_ms} ms`}>{tr("Active")}</span>
                          {r.entry.kind === "socks5" && r.udp_ms != null && (
                            <span className="status-pill status-udp" title={tr("UDP relay works ({0} ms)", r.udp_ms)}>UDP</span>
                          )}
                          {r.country && (
                            <span className="bulk-country">
                              <CountryFlag cc={r.country} />
                              <span className="flag bulk-country-code">{r.country}</span>
                            </span>
                          )}
                        </>
                      )}
                      {r.status === "fail" && (
                        <span className="status-pill status-failed status-unavailable" title={r.error}>{tr("Failed")}</span>
                      )}
                      {r.status === "incomplete" && (
                        <span className="status-pill" title={r.error}>{tr("Incomplete")}</span>
                      )}
                    </div>
                    <button className="btn-sm btn-ghost icon-only" onClick={() => testOne(i)} disabled={r.status === "testing"} title={tr("Test this row")}><Icon.Refresh size={16} /></button>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>{tr("Cancel")}</button>
          {rows.length === 0 ? (
            <button className="btn-primary" onClick={parse}>{tr("Parse →")}</button>
          ) : (
            <button className="btn-primary" onClick={saveSelected}>
              <Download /> {tr("Import")} {selCount}
            </button>
          )}
        </footer>
      </div>
    </DialogBackdrop>
  );
}

function ProxyEditor({ initial, onClose }: { initial: ProxyEntry; onClose: () => void }) {
  const [p, setP] = useState<ProxyEntry>({ ...initial, username: "", password: "" });
  const [credentialsMode, setCredentialsMode] = useState<"keep" | "replace" | "clear">("keep");
  const nameError = displayNameError(p.name, initial.name, true);
  const save = async () => {
    if (nameError) return;
    try {
      const credentials: ProxyCredentialsUpdate = credentialsMode === "replace"
        ? { mode: "replace", username: p.username ?? "", password: p.password ?? "" }
        : { mode: credentialsMode };
      const request: ProxySaveRequest = {
        id: p.id,
        name: p.name,
        kind: p.kind,
        host: p.host,
        port: p.port,
        country: p.country,
        notes: p.notes,
        credentials,
      };
      await invoke("proxy_save", { entry: request });
      toast.ok(initial.id ? tr("Proxy saved") : tr("Proxy added"));
      onClose();
    } catch (e) { toast.err(String(e)); }
  };
  const clearCredentials = () => {
    setP({ ...p, username: "", password: "", credentials_configured: false, credentials_unavailable: false });
    setCredentialsMode("clear");
  };
  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog">
        <header className="dialog-head">
          <h2>{initial.id ? tr("Edit proxy") : tr("New proxy")}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          <Field label={tr("Name")} value={p.name} onChange={(v: string) => setP({ ...p, name: v })} error={nameError} />
          <div className="form-row">
            <label>
              <span className="lbl">{tr("Type")}</span>
              <select value={p.kind} onChange={(e) => setP({ ...p, kind: e.target.value as ProxyEntry["kind"] })}>
                <option value="socks5">SOCKS5</option><option value="http">HTTP</option><option value="https">HTTPS</option>
              </select>
            </label>
            <Field label={tr("Country")} value={p.country} onChange={(v: string) => setP({ ...p, country: v })} />
          </div>
          <div className="form-row">
            <Field label={tr("Host")} value={p.host} onChange={(v: string) => setP({ ...p, host: v })} />
            <NumField label={tr("Port")} value={p.port} onChange={(v) => setP({ ...p, port: v as any })} />
          </div>
          <div className="form-row">
            <Field
              label={tr("Username")}
              value={p.username ?? ""}
              onChange={(v: string) => {
                setP({ ...p, username: v, credentials_unavailable: false });
                setCredentialsMode("replace");
              }}
            />
            <Field
              label={tr("Password")}
              value={p.password ?? ""}
              onChange={(v: string) => {
                setP({ ...p, password: v, credentials_unavailable: false });
                setCredentialsMode("replace");
              }}
              type="password"
            />
          </div>
          {(p.credentials_configured || p.credentials_unavailable) && credentialsMode === "keep" && (
            <button className="btn-ghost" type="button" onClick={clearCredentials}>{tr("Clear saved credentials")}</button>
          )}
          {p.credentials_unavailable && credentialsMode === "keep" && <p className="muted">{tr("Credentials belong to another Windows user. Re-enter both values before testing or launching.")}</p>}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>{tr("Cancel")}</button>
          <button className="btn-primary" onClick={save} disabled={nameError != null} title={nameError ?? undefined}><Save /> {tr("Save")}</button>
        </footer>
      </div>
    </DialogBackdrop>
  );
}

function FingerprintsView() {
  const [items, setItems] = useState<FingerprintEntry[]>([]);
  const [importerOpen, setImporterOpen] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);

  const reload = async () => {
    try { setItems(await invoke<FingerprintEntry[]>("fingerprint_list")); setLoadError(null); }
    catch (e) { setLoadError(String(e)); }
    finally { setLoading(false); }
  };
  useEffect(() => { reload(); }, []);

  const use = async (id: string) => {
    try {
      const meta = await invoke<ProfileMeta>("profile_create_from_template", { templateId: id });
      toast.ok(tr("Created \"{0}\" — open Browsers to edit", meta.name));
    } catch (e) {
      toast.err(String(e));
    }
  };

  const remove = async (id: string) => {
    if ((await confirmModal({ title: tr("Remove fingerprint"), message: tr("Remove this fingerprint from the library?"), danger: true })) !== true) return;
    try {
      await invoke("fingerprint_delete", { id });
      toast.ok(tr("Removed"));
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const importJsonFile = async () => {
    try {
      const txt = await pickJsonText();
      if (txt === null) return;
      const e = await invoke<FingerprintEntry>("fingerprint_import", { jsonText: txt, idHint: null });
      toast.ok(tr("Imported \"{0}\"", e.label));
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  return (
    <section className="page">
      <PageToolbar title={tr("Fingerprint Library")}>
          <button
            className="btn-ghost"
            onClick={async () => {
              try {
                await invoke("open_fingerprint_dir");
              } catch (e) { toast.err(String(e)); }
            }}
            title={tr("Reveal the on-disk library folder; drop JSONs here to add them")}
          >
            <FolderOpen /> {tr("Library folder")} </button>
          <button className="btn-ghost" onClick={importJsonFile}><FileUp /> {tr("Import from file")}</button>
          <button className="btn-primary" onClick={() => setImporterOpen(true)}><Plus /> {tr("Paste JSON")}</button>
      </PageToolbar>
      {loadError !== null && <ListState kind="error" text={tr("Unable to load fingerprints: {0}", loadError)} />}
      {loading ? <ListState kind="loading" text={tr("Loading…")} /> : items.length > 0
        ? <LibraryGroups items={items} onUse={use} onRemove={remove} />
        : loadError === null && <ListState kind="empty" text={tr("No fingerprints yet")} />}
      {importerOpen && (
        <FingerprintImporter onClose={() => { setImporterOpen(false); reload(); }} />
      )}
    </section>
  );
}

/// Library entries grouped by OS (macOS → Windows → Linux → other).
function LibraryGroups({
  items, onUse, onRemove,
}: {
  items: FingerprintEntry[];
  onUse: (id: string) => void;
  onRemove: (id: string) => void;
}) {
  const groups = useMemo(() => {
    const order = ["macOS", "Windows", "Linux"];
    const buckets = new Map<string, FingerprintEntry[]>();
    for (const it of items) {
      const k = it.platform || "Other";
      if (!buckets.has(k)) buckets.set(k, []);
      buckets.get(k)!.push(it);
    }
    return [
      ...order.filter((k) => buckets.has(k)).map((k) => [k, buckets.get(k)!] as const),
      ...[...buckets.keys()].filter((k) => !order.includes(k)).map((k) => [k, buckets.get(k)!] as const),
    ];
  }, [items]);

  return (
    <div className="lib-groups">
      {groups.map(([platform, list]) => (
        <div key={platform} className="lib-group">
          <div className="lib-group-head">
            <span className={`lib-group-dot lib-dot-${platform.toLowerCase()}`} />
            <h3>{tr(platform)}</h3>
            <span className="lib-group-count">{list.length}</span>
          </div>
          <div className="library-grid">
            {list.map((t) => (
              <article
                key={t.id}
                className="library-card"
              >
                <header className="library-card-head">
                  <h3 title={t.label}>{t.label}</h3>
                  {t.chrome && <span className="library-tag">Chrome {t.chrome}</span>}
                </header>
                <p className="muted small">{t.gpu || "—"}</p>
                <footer className="library-card-foot">
                  <button className="btn-sm btn-ghost" onClick={() => onUse(t.id)}><Plus /> {tr("Use")}</button>
                  {t.builtin
                    ? <span className="library-tag">{tr("built-in")}</span>
                    : <button className="icon-btn danger" onClick={() => onRemove(t.id)} title={tr("Remove")} aria-label={tr("Remove")}><Trash /></button>}
                </footer>
              </article>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

function FingerprintImporter({ onClose }: { onClose: () => void }) {
  const [text, setText] = useState("");
  const [name, setName] = useState("");
  const save = async () => {
    try {
      const e = await invoke<FingerprintEntry>("fingerprint_import", { jsonText: text, idHint: name || null });
      toast.ok(tr("Imported \"{0}\"", e.label));
      onClose();
    } catch (e) { toast.err(String(e)); }
  };
  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog dialog-wide">
        <header className="dialog-head">
          <h2>{tr("Paste FingerprintConfig JSON")}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          <Field label={tr("Name (optional, becomes the file id)")} value={name} onChange={setName} placeholder={tr("e.g. mac-m4-pro-real")} />
          <label>
            <span className="lbl">{tr("Paste the full JSON")}</span>
            <textarea rows={14} className="mono" value={text} onChange={(e) => setText(e.target.value)} placeholder='{ "name": "...", "navigator": { ... }, "webgl": { ... }, ... }' />
          </label>
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>{tr("Cancel")}</button>
          <button className="btn-primary" onClick={save}><Download /> {tr("Import")}</button>
        </footer>
      </div>
    </DialogBackdrop>
  );
}

/// Folder picker/creator modal (replaces native prompt). mode: "create" | "move".
function FolderModal({
  mode, existing, onPick, onCreate, onClose,
}: {
  mode: "create" | "move";
  existing: string[];
  onPick: (folder: string) => void;
  onCreate: (name: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => { ref.current?.focus(); }, []);
  const trimmed = name.trim();
  const dup = existing.includes(trimmed);
  const create = () => { if (trimmed && !dup) onCreate(trimmed); };
  const showList = mode === "move" && existing.length > 0;
  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog">
        <header className="dialog-head">
          <h2>{mode === "move" ? tr("Move to folder") : tr("New folder")}</h2>
          <button className="icon-btn" onClick={onClose} aria-label={tr("Close")} title={tr("Close")}><X /></button>
        </header>
        <div className="dialog-body">
          {showList && (
            <>
              <span className="lbl">{tr("Existing folders")}</span>
              <div className="folder-pick-list">
                {existing.map((f) => (
                  <button key={f} className="folder-pick" onClick={() => onPick(f)}>
                    <Icon.Folder size={16} /> {f}
                  </button>
                ))}
              </div>
              <div className="folder-pick-sep"><span>{tr("or create new")}</span></div>
            </>
          )}
          <label>
            <span className="lbl">{showList ? tr("New folder name") : tr("Folder name")}</span>
            <input
              ref={ref}
              value={name}
              placeholder={tr("e.g. Shops, Socials, QA…")}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") create();
                if (e.key === "Escape") onClose();
              }}
            />
          </label>
          {dup && <div className="muted small" style={{ color: "var(--err)" }}>{tr("Folder “")}{trimmed}{tr("” already exists.")}</div>}
          <div style={{ display: "flex", justifyContent: "flex-end", gap: 10, marginTop: 4 }}>
            <button className="btn-ghost" onClick={onClose}>{tr("Cancel")}</button>
            <button className="btn-primary" disabled={!trimmed || dup} onClick={create}>
              {showList ? tr("Create & move") : tr("Create")}
            </button>
          </div>
        </div>
      </div>
    </DialogBackdrop>
  );
}

/// First-run gate: fullscreen overlay until runtime is on disk.
type RtSpec = {
  browser: { key: string; label: string };
  widevine: { key: string; label: string } | null;
};
type RtStatus = {
  installed: boolean;
  binary_path: string | null;
  initialized: boolean;
  spec: RtSpec | null;
  fingerprints_installed: boolean;
  widevine_installed: boolean;
  migration_pending: boolean;
};
type RtProgress = {
  label: string;
  phase: "download" | "extract";
  received: number;
  total: number;
  percent: number;
};

type RtGatePhase = "checking" | "repair-required" | "installing" | "ready";
type RtInstallMode = "setup" | "repair";

function FirstRunGate({ children }: { children: ReactNode }) {
  const [phase, setPhase] = useState<RtGatePhase>("checking");
  const [installMode, setInstallMode] = useState<RtInstallMode>("setup");
  const [repairAttempt, setRepairAttempt] = useState(0);
  const [showChecking, setShowChecking] = useState(false);
  const [prog, setProg] = useState<RtProgress | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const fmt = (b: number) =>
    b < 1024 * 1024 ? `${(b / 1024).toFixed(0)} KB` : `${(b / (1024 * 1024)).toFixed(1)} MB`;

  // Most local checks finish before the first useful paint. Only reveal a
  // neutral startup state when disk access is unusually slow.
  useEffect(() => {
    const timer = window.setTimeout(() => setShowChecking(true), 200);
    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unProg: (() => void) | undefined;

    const installRuntime = async (mode: RtInstallMode, force: boolean) => {
      if (cancelled) return;
      setInstallMode(mode);
      setPhase("installing");
      setErr(null);
      setProg(null);

      let stopProgress: (() => void) | undefined;
      try {
        // Subscribe immediately before install so the local startup check never
        // waits on event registration, while still preserving the first event.
        stopProgress = await listen<RtProgress>("runtime:progress", (e) => {
          if (!cancelled) setProg(e.payload);
        });
        if (cancelled) return;

        const result = await invoke<RtStatus>("runtime_install", { force });
        if (cancelled) return;
        if (!result.installed || !result.fingerprints_installed || !result.widevine_installed) {
          throw new Error(tr("Browser runtime installation did not complete"));
        }
        setProg(null);
        setPhase("ready");
      } catch (e: any) {
        if (!cancelled) {
          setProg(null);
          setErr(typeof e === "string" ? e : (e?.message ?? String(e)));
          setPhase("repair-required");
        }
      } finally {
        stopProgress?.();
        if (unProg === stopProgress) unProg = undefined;
      }
    };

    (async () => {
      // Remove the pending marker used by older builds. Runtime updates are now
      // checked only from Settings and are never installed automatically.
      localStorage.removeItem("shardx-runtime-update-pending");
      let status: RtStatus;
      try {
        const recovery = await invoke<{ incomplete: boolean }>("process_recovery_status");
        if (recovery.incomplete) { if (!cancelled) setPhase("ready"); return; }
        status = await invoke<RtStatus>("runtime_local_status");
      } catch (e: any) {
        if (!cancelled) { setErr(String(e)); setPhase("repair-required"); }
        return;
      }
      if (cancelled) return;

      // Unsupported platform: let the user in; launch will error if attempted.
      if (!status.spec) {
        setPhase("ready");
        return;
      }

      const complete = status.installed
        && status.fingerprints_installed
        && status.widevine_installed
        && !status.migration_pending;
      if (complete) {
        setPhase("ready");
        return;
      }

      // Only a genuinely new installation starts downloading automatically.
      // A previously initialized but damaged runtime waits for explicit repair.
      if (!status.initialized && !status.installed) {
        await installRuntime("setup", false);
      } else if (repairAttempt > 0) {
        await installRuntime("repair", true);
      } else {
        setInstallMode("repair");
        setPhase("repair-required");
      }
    })();

    return () => {
      cancelled = true;
      unProg?.();
    };
  }, [repairAttempt]);

  if (phase === "ready") {
    return <>{children}</>;
  }

  if (phase === "checking" && !showChecking && !err) return null;

  const copy = phase === "checking"
    ? {
        title: tr("Starting ShardX"),
        description: tr("Checking the local browser runtime…"),
      }
    : phase === "repair-required"
      ? {
          title: tr("Browser runtime needs repair"),
          description: tr("Required local files are missing or incomplete. Nothing will be downloaded until you choose Repair."),
        }
      : installMode === "repair"
        ? {
            title: tr("Repairing ShardX browser"),
            description: tr("Restoring missing browser runtime files."),
          }
        : {
            title: tr("Setting up ShardX browser"),
            description: tr("Downloading the browser runtime for this installation."),
          };

  return (
    <div className="runtime-gate">
      <div className="runtime-gate-card">
        <div className="runtime-gate-mark"><ShardMini /></div>
        <div className="runtime-gate-title">{copy.title}</div>
        <div className="runtime-gate-description">{copy.description}</div>

        {prog && (
          <div className="runtime-progress-wrap">
            <div className="runtime-progress-label">
              <span>{tr(prog.label)}</span>
              <span>
                {prog.phase === "download"
                  ? prog.total > 0
                    ? `${fmt(prog.received)} / ${fmt(prog.total)} (${prog.percent}%)`
                    : tr("Starting download…")
                  : tr("Extracting…")}
              </span>
            </div>
            <div className="runtime-progress-track">
              <div className="runtime-progress-fill" style={{ width: `${prog.percent}%` }} />
            </div>
          </div>
        )}
        {!prog && !err && phase === "installing" && (
          <div className="runtime-gate-status">
            <span className="runtime-spinner" aria-hidden="true" />
             {tr("Preparing browser runtime…")} </div>
        )}
        {err && (
          <div className="runtime-gate-error">{err}</div>
        )}
        {phase === "repair-required" && (
          <button className="btn-primary runtime-repair-btn" onClick={() => setRepairAttempt((attempt) => attempt + 1)}>
            <Icon.Refresh size={16} /> {err ? tr("Retry browser runtime repair") : tr("Repair browser runtime")}
          </button>
        )}
      </div>
    </div>
  );
}

type RuntimeUpdateCheck = {
  chromium_installed: boolean;
  chromium_installed_version: string | null;
  chromium_latest_version: string | null;
  chromium_update_available: boolean;
  chromium_download_url: string | null;
  fingerprints_installed: boolean;
  fingerprints_check_available: boolean;
  fingerprints_update_available: boolean;
  fingerprints_download_url: string | null;
  widevine_installed: boolean;
  widevine_check_available: boolean;
  widevine_update_available: boolean;
  widevine_download_url: string | null;
};

type RuntimeUpdateCheckSnapshot = {
  last_attempt_at: string | null;
  last_success_at: string | null;
  result: RuntimeUpdateCheck | null;
  error: string | null;
};

type RuntimeUpdateTone = "unchecked" | "current" | "available" | "missing" | "unavailable";

function RuntimeUpdateRow({
  label,
  detail,
  url,
  tone,
  onUpdate,
  updating,
}: {
  label: string;
  detail: string;
  url?: string | null;
  tone: RuntimeUpdateTone;
  onUpdate?: () => void;
  updating?: boolean;
}) {
  const status = tone === "current"
    ? tr("Up to date")
    : tone === "available"
      ? tr("Update available")
      : tone === "missing"
        ? tr("Missing files")
        : tone === "unavailable"
          ? tr("Check unavailable")
          : tr("Not checked");
  return (
    <div className="runtime-update-row">
      <div className="runtime-update-copy">
        <span className="runtime-update-label">{label}</span>
        <span className="runtime-update-detail">{detail}</span>
      </div>
      {url && (
        <a
          className="runtime-update-url"
          href={url}
          title={url}
          onClick={(e) => {
            e.preventDefault();
            openUrl(url).catch(() => {});
          }}
        >
          {url}
        </a>
      )}
      <div className="runtime-update-actions">
        {tone === "available" && onUpdate && (
          <button
            className="runtime-update-apply"
            onClick={onUpdate}
            disabled={updating}
          >
            {updating ? tr("Updating…") : tr("Update")}
          </button>
        )}
        <span className={`runtime-update-state runtime-update-${tone}`}>{status}</span>
      </div>
    </div>
  );
}

function RuntimeUpdateCard() {
  const [checking, setChecking] = useState(false);
  const [updating, setUpdating] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<RuntimeUpdateCheckSnapshot | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const result = snapshot?.result ?? null;

  useEffect(() => {
    let disposed = false;
    const keepNewestSnapshot = (next: RuntimeUpdateCheckSnapshot) => {
      if (disposed) return;
      setSnapshot((current) => {
        const currentAttempt = current?.last_attempt_at ?? "";
        const nextAttempt = next.last_attempt_at ?? "";
        return currentAttempt > nextAttempt ? current : next;
      });
    };
    const unlisten = listen<RuntimeUpdateCheckSnapshot>("runtime:update-check-complete", (event) => {
      keepNewestSnapshot(event.payload);
    });
    unlisten
      .then(() => invoke<RuntimeUpdateCheckSnapshot>("runtime_update_check_status"))
      .then(keepNewestSnapshot)
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten.then((stop) => stop());
    };
  }, []);

  const check = async () => {
    setChecking(true);
    setActionError(null);
    try {
      setSnapshot(await invoke<RuntimeUpdateCheckSnapshot>("runtime_check_updates"));
    } catch (e: any) {
      setActionError(typeof e === "string" ? e : (e?.message ?? String(e)));
    } finally {
      setChecking(false);
    }
  };

  const applyUpdate = async (component: string) => {
    setUpdating(component);
    setActionError(null);
    try {
      await invoke("runtime_apply_updates", { component });
      await check();
    } catch (e: any) {
      setActionError(typeof e === "string" ? e : (e?.message ?? String(e)));
    } finally {
      setUpdating(null);
    }
  };

  const tone = (installed: boolean, available: boolean, checkAvailable = true): RuntimeUpdateTone => {
    if (!result) return "unchecked";
    if (!installed) return "missing";
    if (!checkAvailable) return "unavailable";
    return available ? "available" : "current";
  };

  const chromiumDetail = result
    ? result.chromium_installed
      ? tr("Installed {0} · Latest {1}", result.chromium_installed_version ?? "unknown", result.chromium_latest_version ?? "unknown")
      : tr("The local Chromium runtime is incomplete.")
    : tr("Compare the installed browser engine with the latest runtime manifest.");
  const lastAttempt = snapshot?.last_attempt_at ?? null;
  const lastSuccess = snapshot?.last_success_at ?? null;
  const checkError = snapshot?.error ?? null;

  return (
    <div className="card settings-card runtime-update-card">
      <div className="runtime-update-head">
        <div className="settings-card-heading">
          <h3>{tr("Runtime update check")}</h3>
          <p className="muted">
             {tr("Checks run automatically at startup and every hour. Updates download and install only when you click Update.")} </p>
          <p className="runtime-update-last-check">
            {lastAttempt
              ? tr("Last checked {0}", fmtTs(lastAttempt))
              : tr("No update check has completed yet.")}
            {lastSuccess && lastSuccess !== lastAttempt ? tr(" Last successful result {0}.", fmtTs(lastSuccess)) : ""}
          </p>
        </div>
        <button className="btn-ghost runtime-update-check" onClick={check} disabled={checking}>
          <Icon.Refresh size={16} /> {checking ? tr("Checking…") : tr("Check for updates")}
        </button>
      </div>

      <div className="runtime-update-list">
        <RuntimeUpdateRow
          label={tr("Chromium browser runtime")}
          detail={chromiumDetail}
          url={result?.chromium_download_url}
          tone={tone(result?.chromium_installed ?? true, result?.chromium_update_available ?? false)}
          onUpdate={() => applyUpdate("chromium")}
          updating={updating === "chromium"}
        />
        <RuntimeUpdateRow
          label={tr("Fingerprint library")}
          detail={result?.fingerprints_installed === false
            ? tr("The bundled fingerprint templates are incomplete.")
            : result?.fingerprints_check_available === false
              ? tr("Unable to confirm the latest fingerprint archive.")
              : tr("Bundled multi-platform fingerprint templates.")}
          url={result?.fingerprints_download_url}
          tone={tone(
            result?.fingerprints_installed ?? true,
            result?.fingerprints_update_available ?? false,
            result?.fingerprints_check_available ?? true,
          )}
          onUpdate={() => applyUpdate("fingerprints")}
          updating={updating === "fingerprints"}
        />
        <RuntimeUpdateRow
          label="Widevine CDM"
          detail={result?.widevine_installed === false
            ? tr("Required Widevine runtime files are incomplete.")
            : result?.widevine_check_available === false
              ? tr("Unable to confirm the latest Widevine archive.")
              : tr("DRM component bundled with the browser runtime.")}
          url={result?.widevine_download_url}
          tone={tone(
            result?.widevine_installed ?? true,
            result?.widevine_update_available ?? false,
            result?.widevine_check_available ?? true,
          )}
          onUpdate={() => applyUpdate("widevine")}
          updating={updating === "widevine"}
        />
      </div>

      {checkError && (
        <div className="runtime-update-error">
          {lastAttempt ? tr("Last check failed {0}: ", fmtTs(lastAttempt)) : tr("Last check failed: ")}{checkError}
        </div>
      )}
      {actionError && <div className="runtime-update-error">{actionError}</div>}
      <div className="runtime-update-note">
         {tr("ShardX Launcher itself is excluded from update checks. Engine and Widevine updates stop running browsers first.")} </div>
    </div>
  );
}

/// Sidebar version pill; reads only the locally installed app version.
function VersionPill() {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    getVersion().then(setVersion).catch(() => {});
  }, []);
  return (
    <button
      type="button"
      className="version-pill"
      disabled
      title={version ? tr("Running {0}", version) : tr("Version unavailable")}
    >
      <ShardMini />
      <div className="version-pill-text">
        <div className="version-pill-current">
          ShardX Launcher v{version ?? "…"}
        </div>
        <div className="version-pill-sub">{tr("update checks disabled")}</div>
      </div>
    </button>
  );
}

function SettingsView({ workspaceBackup }: { workspaceBackup: ReturnType<typeof useWorkspaceBackup> }) {
  const [s, setS] = useState<Settings>({
    theme: "dark",
    geo_checker: "ipwho.is",
    screen_resolution_mode: "fingerprint",
    api_enabled: true,
    api_port: 40325,
  });
  const [api, setApi] = useState<ApiInfo | null>(null);
  const refreshApi = () => invoke<ApiInfo>("api_info").then(setApi).catch(e => toast.err(String(e)));
  useStoreChanged(refreshApi, ["api"]);
  // Snapshot of the last loaded/saved settings; Save stays disabled until the
  // form actually differs from it.
  const [saved, setSaved] = useState<Settings | null>(null);
  useEffect(() => { invoke<Settings>("settings_get").then((v) => { setS(v); setSaved(v); }); refreshApi(); }, []);
  const regenToken = async () => {
    try {
      const next = await invoke<ApiInfo>("api_regenerate_token");
      setApi(next);
      const refreshed = await invoke<Settings>("settings_get");
      setS(refreshed);
      setSaved(refreshed);
      toast.ok(tr("Token regenerated"));
    }
    catch (e) { toast.err(String(e)); }
  };

  const [mcpBusy, setMcpBusy] = useState(false);
  // Download MCP server source into the portable app data directory.
  const downloadMcp = async () => {
    setMcpBusy(true);
    try {
      const path = await invoke<string>("mcp_download");
      toast.ok(tr("MCP downloaded to {0}", path));
    } catch (e) { toast.err(tr("MCP download failed: ") + String(e)); }
    finally { setMcpBusy(false); }
  };
  const save = async () => {
    try { await invoke("settings_save", { value: s }); setSaved(s); toast.ok(tr("Settings saved")); }
    catch (e) { toast.err(String(e)); }
  };
  const settingsDirty = saved !== null && JSON.stringify(s) !== JSON.stringify(saved);
  return (
    <section className="page settings-page">
      <PageToolbar title={tr("Settings")}>
        <button className="btn-primary" onClick={async () => { await save(); refreshApi(); }} disabled={!settingsDirty}><Save /> {tr("Save settings")}</button>
      </PageToolbar>

      <div className="settings-card-list">
        <WorkspaceBackup {...workspaceBackup} />
        <div className="card settings-card">
          <h3>{tr("Proxy geo checker")}</h3>
          <p className="muted">{tr("Preferred free public IP-geo service for manual and scheduled proxy tests. If it times out, returns HTTP 429, or sends invalid JSON, ShardX automatically tries the next provider.")}</p>
          <label>
            <span className="lbl">{tr("Preferred provider")}</span>
            <select value={s.geo_checker ?? "ipwho.is"} onChange={(e) => setS({ ...s, geo_checker: e.target.value })}>
              <option value="ipwho.is">{tr("ipwho.is (1k/day, HTTPS)")}</option>
              <option value="geojs.io">{tr("geojs.io (no rate limits, HTTPS)")}</option>
              <option value="country.is">{tr("country.is (10 req/s, HTTPS)")}</option>
              <option value="bigdatacloud.com">{tr("BigDataCloud (client-side fair use, HTTPS)")}</option>
              <option value="freeipapi.com">{tr("freeipapi.com (60 req/min, HTTPS)")}</option>
              <option value="ipapi.is">{tr("ipapi.is (1k/day, HTTPS)")}</option>
            </select>
          </label>
        </div>

      <div className="card settings-card">
        <h3>{tr("Screen resolution")}</h3>
        <p className="muted">
          <strong>{tr("From fingerprint")}</strong>  {tr("reports the screen carried in the bound profile (recommended for anti-detect coherence).")} <strong>  {tr("Real")}</strong>  {tr("lets ShardX expose the host monitor's actual size.")} </p>
        <label>
          <span className="lbl">{tr("Mode")}</span>
          <select
            value={s.screen_resolution_mode ?? "fingerprint"}
            onChange={(e) => setS({ ...s, screen_resolution_mode: e.target.value })}
          >
            <option value="fingerprint">{tr("From fingerprint")}</option>
            <option value="real">{tr("Real (host monitor)")}</option>
          </select>
        </label>
      </div>

      <div className="card settings-card">
        <h3>{tr("Automation API")}</h3>
        <p className="muted">
           {tr("Local HTTP API (axum) for scripting — create/launch/close profiles and get a CDP WebSocket URL. Binds")} <strong>127.0.0.1</strong>  {tr("only, JWT Bearer auth. Changes to enable/port apply after restarting the app.")}{" "}
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              openUrl(withUtm("https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true")).catch(() => {});
            }}
          >
             {tr("Full API reference →")} </a>
        </p>
        <label className="row-inline">
          <input
            type="checkbox"
            checked={s.api_enabled ?? true}
            onChange={(e) => setS({ ...s, api_enabled: e.target.checked })}
          />
          <span className="lbl">{tr("Enable API server")}</span>
        </label>
        <label>
          <span className="lbl">{tr("Port")}</span>
          <input
            type="number"
            value={s.api_port ?? 40325}
            onChange={(e) => setS({ ...s, api_port: Number(e.target.value) || 40325 })}
          />
        </label>
        {api && (
          <>
            <p role="status">{api.listener.state === "listening" ? tr("Listening on port {0}", api.listener.port ?? "") : api.listener.state === "failed" ? tr("API could not start: {0}", api.listener.error ?? "") : tr("API is not listening")}</p>
            {(api.enabled !== (api.listener.state === "listening") || (api.enabled && api.port !== api.listener.port)) && <p className="muted small">{tr("Saved API settings take effect after restarting the launcher.")}</p>}
            <label>
              <span className="lbl">{tr("Base URL")}</span>
              <CopyField value={api.base_url} />
            </label>
            <label>
              <span className="lbl">{tr("Bearer token")}</span>
              <CopyField value={api.token} secret />
            </label>
            <div className="row-inline settings-card-inline-action">
              <button className="btn-ghost" onClick={regenToken}>{tr("Regenerate token")}</button>
              <span className="muted small">{tr("Invalidates the current token immediately.")}</span>
            </div>
            <p className="muted small settings-card-footnote">
               {tr("Send it as")} <code>Authorization: Bearer &lt;token&gt;</code>.
            </p>
          </>
        )}
      </div>

      <div className="card settings-card">
        <h3>{tr("MCP server")}</h3>
        <p className="muted">
           {tr("Download the")} <strong>MCP</strong>  {tr("server source (lets an AI client drive profiles and a CDP browser) into a folder you choose. The app does not run it — install its deps and register it with your MCP client per the included README. Requires Node.js.")} </p>
        <button className="btn-ghost" onClick={downloadMcp} disabled={mcpBusy}>
          <Icon.Download size={16} /> {mcpBusy ? tr("Downloading…") : tr("Download MCP server")}
        </button>
      </div>

        <RuntimeUpdateCard />
      </div>

    </section>
  );
}
