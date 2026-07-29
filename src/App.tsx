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
  closestCenter,
  pointerWithin,
  useDroppable,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragOverEvent,
  type CollisionDetection,
} from "@dnd-kit/core";
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./fonts.css";
import "./App.css";

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

type ProfileBackupImportFile = { name: string; text: string };
type ProfileBackupSummary = { profileCount: number; cookieCount: number };
const MAX_PROFILE_BACKUP_BYTES = 64 * 1024 * 1024;
const MAX_PROFILE_BACKUP_BATCH = 100;

/// Select one or more complete profile backups. The filename is retained so
/// Rust can enforce the custom extension as well as the versioned file body.
const pickProfileBackupFiles = () => new Promise<ProfileBackupImportFile[] | null>((resolve, reject) => {
  const input = document.createElement("input");
  input.type = "file";
  input.accept = ".shardx-backup";
  input.multiple = true;
  input.style.display = "none";
  input.addEventListener("change", async () => {
    const selected = [...(input.files ?? [])];
    if (selected.length === 0) {
      input.remove();
      resolve(null);
      return;
    }
    try {
      if (selected.length > MAX_PROFILE_BACKUP_BATCH) {
        throw new Error("At most 100 profiles can be imported at once");
      }
      const invalid = selected.find((file) => !file.name.toLowerCase().endsWith(".shardx-backup"));
      if (invalid) throw new Error(`${invalid.name} is not a .shardx-backup file`);
      const oversized = selected.find((file) => file.size > MAX_PROFILE_BACKUP_BYTES);
      if (oversized) throw new Error(`${oversized.name} exceeds the 64 MB backup limit`);
      resolve(await Promise.all(selected.map(async (file) => ({ name: file.name, text: await file.text() }))));
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

// Single UTM tag appended to every outbound proxyshard.com link.
const UTM_QS = "utm_source=shardx&utm_medium=referral&utm_campaign=shardx-launcher";
const withUtm = (url: string) => url + (url.includes("?") ? "&" : "?") + UTM_QS;
// Docs URL behind the proxy UDP/No-UDP pill.
const UDP_DOCS_URL = withUtm("https://docs.proxyshard.com/eng/our-products/about-udp");

// ---- toasts (global queue, auto-expiry; push via toast.ok / toast.err) ----

type ToastItem = { id: number; kind: "ok" | "err" | "info"; text: string; closing: boolean };
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
  err: (t: string) => pushToast("err", t),
  info: (t: string) => pushToast("info", t),
};

function ToastGlyph({ kind }: { kind: ToastItem["kind"] }) {
  return (
    <svg viewBox="0 0 14 14" aria-hidden="true">
      {kind === "ok" ? (
        <path d="M2.8 7.2 5.6 10 11.2 4.4" />
      ) : kind === "err" ? (
        <path d="m4 4 6 6m0-6-6 6" />
      ) : (
        <>
          <circle cx="7" cy="4.1" r="0.8" />
          <path d="M7 6.4v3.7" />
        </>
      )}
    </svg>
  );
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
        { label: "Cancel", value: false },
        { label: opts.danger ? "Delete" : "OK", value: true, danger: opts.danger, primary: !opts.danger },
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
          <h2>{req.title ?? "Confirm"}</h2>
          <button className="icon-btn" onClick={() => done(null)}>✕</button>
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
  username: string;
  password: string;
  country: string;
  notes: string;
};
type Settings = {
  theme: string;
  geo_checker?: string | null;
  screen_resolution_mode?: string | null;
  api_enabled?: boolean;
  api_port?: number;
  api_secret?: string;
};
type ApiInfo = {
  enabled: boolean;
  port: number;
  base_url: string;
  token: string;
};
type Section = "browsers" | "proxies" | "fingerprints" | "settings";

/// Library fingerprint backing the editor GPU select; payload supplies the coherent base.
type FingerprintEntry = {
  id: string;
  label: string;
  platform: string;
  chrome: string;
  gpu: string;
  tag_color: string;
  builtin: boolean;
  payload: any;
};

// ---- profile form ----

const PROFILE_NAME_PATTERN = /^[A-Za-z0-9_-]+$/;
function profileNameError(name: string): string | null {
  if (!name.trim()) return "Profile name is required";
  if (!PROFILE_NAME_PATTERN.test(name)) {
    return "Use only letters, numbers, underscores (_), and hyphens (-)";
  }
  return null;
}

type NoiseMode = "real" | "auto";
type WebRtcMode = "auto" | "tcp_only" | "block";
type GeoMode = "auto" | "manual";

type ProfileForm = {
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

  noise_canvas: "real",
  noise_webgl: "real",
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
  f.proxy_id = stored?._meta?.proxy_id ?? null;
  f.name = stored?.name ?? "";
  f.notes = stored?.notes ?? "";
  // Empty for legacy profiles; snapped by useEffect.
  f.gpu_preset_id = stored?._meta?.gpu_preset_id ?? "";
  f.user_agent = stored?.navigator?.user_agent ?? f.user_agent;
  f.hardware_concurrency = stored?.navigator?.hardware_concurrency ?? 8;
  f.device_memory = stored?.navigator?.device_memory ?? 16;
  f.timezone = stored?.timezone ?? AUTO_TZ;
  f.language = stored?.navigator?.language ?? AUTO_LANG;
  f.webrtc = (stored?.webrtc === "replace" ? "tcp_only" : stored?.webrtc) ?? "block";
  f.do_not_track = !!stored?.navigator?.do_not_track;

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
function toStored(f: ProfileForm, lib: FingerprintEntry | null, existing: any | null = null): any {
  const existingPresetId = existing?._meta?.gpu_preset_id ?? "";
  const source = f.id && existing && existingPresetId === f.gpu_preset_id
    ? existing
    : lib?.payload;
  const base: any = source ? JSON.parse(JSON.stringify(source)) : {};

  base._meta = {
    id: f.id,
    proxy_id: f.proxy_id,
    last_launched_at: null,
    gpu_preset_id: f.gpu_preset_id,
  };
  base.name = f.name;
  base.notes = f.notes;
  // "auto" sentinel: resolver replaces at launch; persists across edits.
  base.timezone = f.timezone;
  base.icu_locale = f.language === AUTO_LANG ? null : f.language;
  base.webrtc = f.webrtc;

  base.navigator = {
    ...(base.navigator || {}),
    language: f.language,
    accept_language: f.language === AUTO_LANG ? null : deriveAcceptLanguage(f.language),
    languages: f.language === AUTO_LANG ? null : deriveLanguagesArray(f.language),
    user_agent: f.user_agent,
    hardware_concurrency: f.hardware_concurrency,
    device_memory: f.device_memory,
    // Empty → inherit donor; set → write to both navigator + client_hints.
    ...(f.platform_version ? { platform_version: f.platform_version } : {}),
    do_not_track: f.do_not_track ? "1" : null,
  };
  if (f.platform_version) {
    base.client_hints = {
      ...(base.client_hints || {}),
      platform_version: f.platform_version,
    };
  }

  base.media_devices = {
    audio_input_count: f.media_audio_in,
    audio_output_count: f.media_audio_out,
    video_input_count: f.media_video_in,
  };

  base.geolocation =
    f.geo_mode === "manual"
      ? { mode: "manual", latitude: f.geo_lat, longitude: f.geo_lng, accuracy: f.geo_accuracy }
      : { mode: "auto" };

  // seed: 0 is the "derive automatically" sentinel — the launcher fills each
  // vector with a stable per-profile seed once the real profile id exists
  // (see fill_noise_seeds in profile.rs).  Computing seeds here is impossible
  // for new profiles (no id yet) and previously collapsed every new profile
  // onto one shared seed, giving them all an identical fingerprint.
  base.noise = {
    canvas:       { enabled: f.noise_canvas === "auto",       seed: 0 },
    webgl:        { enabled: f.noise_webgl === "auto",        seed: 0, intensity: f.noise_webgl === "auto" ? WEBGL_NOISE_INTENSITY : 0 },
    audio:        { enabled: f.noise_audio === "auto",        seed: 0 },
    client_rects: { enabled: f.noise_client_rects === "auto", seed: 0, max_offset: f.noise_client_rects === "auto" ? CLIENT_RECTS_MAX_OFFSET : 0 },
    sensors:      { enabled: f.noise_sensors === "auto",      seed: 0 },
    fonts:        { enabled: f.noise_fonts === "auto",        seed: 0 },
  };
  base.blocked_ports = [...f.blocked_ports].sort((a, b) => a - b);

  return base;
}

// ---- app shell ----

type Theme = "dark" | "light";

export default function App() {
  const [section, setSection] = useState<Section>("browsers");
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem("shardx-theme") as Theme) || "dark",
  );
  useEffect(() => {
    let disposed = false;
    let unlisten: undefined | (() => void);
    listen<{ running_count: number }>("launcher:exit-blocked", ({ payload }) => {
      const count = Math.max(1, payload.running_count);
      const plural = count === 1 ? "browser is" : "browsers are";
      void confirmModal({
        title: "Close running browsers first",
        message: `ShardX cannot exit while ${count} launched ${plural} still running. Close ${count === 1 ? "it" : "them"} first, then try again.`,
        buttons: [{ label: "OK", value: true, primary: true }],
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
              aria-label="Minimize"
              onClick={() => getCurrentWindow().minimize()}
            >
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                <line x1="1" y1="5" x2="9" y2="5" stroke="currentColor" strokeWidth="1" />
              </svg>
            </button>
            <button
              className="tb-btn"
              aria-label="Maximize"
              onClick={() => getCurrentWindow().toggleMaximize()}
            >
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                <rect x="1.5" y="1.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" />
              </svg>
            </button>
            <button
              className="tb-btn tb-close"
              aria-label="Close"
              onClick={() => getCurrentWindow().close()}
            >
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                <line x1="1" y1="1" x2="9" y2="9" stroke="currentColor" strokeWidth="1" />
                <line x1="9" y1="1" x2="1" y2="9" stroke="currentColor" strokeWidth="1" />
              </svg>
            </button>
          </div>
        )}
      </div>
      <FirstRunGate>
        <div className="app">
          <Sidebar
            section={section}
            onSelect={setSection}
            theme={theme}
            onToggleTheme={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
          />
          <main className="main">
            {section === "browsers" && <BrowsersView />}
            {section === "proxies" && <ProxiesView />}
            {section === "fingerprints" && <FingerprintsView />}
            {section === "settings" && <SettingsView />}
          </main>
          <ToastHost />
          <ConfirmHost />
        </div>
      </FirstRunGate>
    </>
  );
}

function Sidebar({
  section, onSelect, theme, onToggleTheme,
}: {
  section: Section;
  onSelect: (s: Section) => void;
  theme: "dark" | "light";
  onToggleTheme: () => void;
}) {
  const sections: { label: string; items: { id: Section; label: string; svg: ReactNode }[] }[] = [
    {
      label: "Workspace",
      items: [
        { id: "browsers", label: "Browsers", svg: <IconShard /> },
      ],
    },
    {
      label: "PROXYLIST",
      items: [{ id: "proxies", label: "Proxies", svg: <IconWire /> }],
    },
    {
      label: "Library",
      items: [{ id: "fingerprints", label: "Fingerprints", svg: <IconHex /> }],
    },
    {
      label: "System",
      items: [{ id: "settings", label: "Settings", svg: <IconCog /> }],
    },
  ];

  // Automation/MCP quick widget (fills the sidebar's lower space).
  const [autoUrl, setAutoUrl] = useState("");
  const [mcpBusy, setMcpBusy] = useState(false);
  useEffect(() => {
    invoke<{ base_url: string; enabled: boolean }>("api_info")
      .then((i) => setAutoUrl(i.enabled ? i.base_url : ""))
      .catch(() => {});
  }, []);
  const downloadMcp = async () => {
    setMcpBusy(true);
    try {
      const p = await invoke<string>("mcp_download");
      toast.ok(`MCP downloaded to ${p}`);
    } catch (e) { toast.err("MCP download failed: " + String(e)); }
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
          <div className="side-auto-head">Automation API</div>
          {autoUrl ? (
            <button
              className="side-auto-url"
              title="Copy API base URL"
              onClick={() => { clip.write(autoUrl); toast.ok("Copied API URL"); }}
            >
              <span className="mono">{autoUrl.replace(/^https?:\/\//, "")}</span>
              <Icon.Clone />
            </button>
          ) : (
            <div className="side-auto-off">API off — enable in Settings</div>
          )}
          <button className="side-auto-btn" onClick={downloadMcp} disabled={mcpBusy}>
            <Icon.Download /> {mcpBusy ? "Downloading…" : "Download MCP"}
          </button>
          <button
            className="side-auto-btn"
            onClick={() => {
              openUrl(withUtm("https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true")).catch(() => {});
            }}
            title="Open the full Automation API reference on docs.proxyshard.com"
          >
            <Icon.Info /> Documentation
          </button>
        </div>
        <button
          className="theme-toggle"
          onClick={onToggleTheme}
          title={theme === "dark" ? "Switch to light theme" : "Switch to dark theme"}
        >
          <span className={`theme-seg ${theme === "light" ? "active" : ""}`}>
            <IconSun /> Light
          </span>
          <span className={`theme-seg ${theme === "dark" ? "active" : ""}`}>
            <IconMoon /> Dark
          </span>
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
const IconShard = () => (
  <svg width="14" height="14" viewBox="0 0 14 14"><path d="M7 1L13 7L7 13L1 7Z" fill="currentColor" /></svg>
);
const IconWire = () => (
  <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
    <path d="M2 4H10M4 10H12M3 4L1 6L3 8M11 6L13 8L11 10" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round"/>
  </svg>
);
const IconHex = () => (
  <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
    <path d="M7 1L12 4V10L7 13L2 10V4Z" stroke="currentColor" strokeWidth="1.4" strokeLinejoin="round"/>
  </svg>
);
const IconCog = () => (
  <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
    <circle cx="7" cy="7" r="2" stroke="currentColor" strokeWidth="1.4"/>
    <path d="M7 1V3M7 11V13M1 7H3M11 7H13M2.5 2.5L4 4M10 10L11.5 11.5M2.5 11.5L4 10M10 4L11.5 2.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round"/>
  </svg>
);
const IconCopy = () => (
  <svg width="13" height="13" viewBox="0 0 14 14" fill="none">
    <rect x="4.4" y="4.4" width="7.2" height="7.2" rx="1.4" stroke="currentColor" strokeWidth="1.3"/>
    <path d="M9.4 2.4H3.1c-.66 0-1.2.54-1.2 1.2v6.3" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
  </svg>
);
/// Read-only value with inline copy glyph.
function CopyField({ value, secret }: { value: string; secret?: boolean }) {
  return (
    <div className="copy-field">
      <input readOnly type={secret ? "password" : "text"} value={value} />
      <button
        type="button"
        className="copy-icon"
        title="Copy"
        onClick={async () => { try { await clip.write(value); toast.ok("Copied"); } catch (e) { toast.err(String(e)); } }}
      >
        <IconCopy />
      </button>
    </div>
  );
}
const IconSun = () => (
  <svg width="13" height="13" viewBox="0 0 14 14" fill="none">
    <circle cx="7" cy="7" r="2.6" stroke="currentColor" strokeWidth="1.3"/>
    <path d="M7 .8V2M7 12v1.2M.8 7H2M12 7h1.2M2.6 2.6l.85.85M10.55 10.55l.85.85M2.6 11.4l.85-.85M10.55 3.45l.85-.85"
          stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
  </svg>
);
const IconMoon = () => (
  <svg width="13" height="13" viewBox="0 0 14 14" fill="none">
    <path d="M12 8.2A5 5 0 1 1 5.8 2 4 4 0 0 0 12 8.2z"
          stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round"/>
  </svg>
);

/// Inline-SVG icon set; stroke-based at 14x14, inherits color, `size` override.
type IconProps = { size?: number; className?: string };
const Icon = {
  Edit: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M10 1.5l2.5 2.5M9 2.5l2.5 2.5M2.5 9l6.5-6.5 2.5 2.5L5 11.5l-3 0.5z"
            stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" strokeLinecap="round"/>
    </svg>
  ),
  Clone: ({ size = 13, className }: IconProps) => (
    // "Duplicate" — two offset rounded rectangles.
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <rect x="4.5" y="4.5" width="8" height="8" rx="1.5" stroke="currentColor" strokeWidth="1.3"/>
      <path d="M2 9.5V2.5C2 1.95 2.45 1.5 3 1.5h7" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
    </svg>
  ),
  More: ({ size = 13, className }: IconProps) => (
    // Vertical kebab — opens the same menu as right-click.
    <svg width={size} height={size} viewBox="0 0 14 14" fill="currentColor" className={className}>
      <circle cx="7" cy="2.5" r="1.3" />
      <circle cx="7" cy="7" r="1.3" />
      <circle cx="7" cy="11.5" r="1.3" />
    </svg>
  ),
  Trash: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M2 3.5h10M5 3.5V2.2C5 1.8 5.3 1.5 5.7 1.5h2.6c0.4 0 0.7 0.3 0.7 0.7v1.3M3.3 3.5l0.7 8.4c0 0.4 0.4 0.6 0.7 0.6h4.6c0.4 0 0.7-0.2 0.7-0.6L10.7 3.5"
            stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round"/>
      <path d="M6 6v4M8 6v4" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
    </svg>
  ),
  Refresh: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M12 7a5 5 0 1 1-1.5-3.5M12 1.5v3h-3" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  ),
  Grip: ({ size = 14, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="currentColor" className={className}>
      <circle cx="4" cy="3" r="1" /><circle cx="10" cy="3" r="1" />
      <circle cx="4" cy="7" r="1" /><circle cx="10" cy="7" r="1" />
      <circle cx="4" cy="11" r="1" /><circle cx="10" cy="11" r="1" />
    </svg>
  ),
  Loader: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <circle cx="7" cy="7" r="5" stroke="currentColor" strokeWidth="1.4" opacity="0.25"/>
      <path d="M7 2a5 5 0 0 1 5 5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round"/>
    </svg>
  ),
  Info: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <circle cx="7" cy="7" r="5.5" stroke="currentColor" strokeWidth="1.3"/>
      <path d="M7 6.3v3.7" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
      <circle cx="7" cy="4.4" r="0.75" fill="currentColor"/>
    </svg>
  ),
  Folder: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M1.5 4.5V3c0-0.5 0.4-1 1-1h3l1.5 1.5h5c0.5 0 1 0.5 1 1V11c0 0.5-0.5 1-1 1H2.5c-0.6 0-1-0.5-1-1V4.5z"
            stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round"/>
    </svg>
  ),
  Upload: ({ size = 13, className }: IconProps) => (
    // Up-arrow into a tray — used for "Export".
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M7 1.5v7M4 4.5l3-3 3 3M2.5 9.5V12c0 0.3 0.2 0.5 0.5 0.5h8c0.3 0 0.5-0.2 0.5-0.5V9.5"
            stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  ),
  Download: ({ size = 13, className }: IconProps) => (
    // Down-arrow into a tray — used for "Import".
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M7 8.5v-7M4 5.5l3 3 3-3M2.5 9.5V12c0 0.3 0.2 0.5 0.5 0.5h8c0.3 0 0.5-0.2 0.5-0.5V9.5"
            stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round"/>
    </svg>
  ),
  Globe: ({ size = 14, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <circle cx="7" cy="7" r="5.5" stroke="currentColor" strokeWidth="1.3"/>
      <path d="M1.5 7h11M7 1.5c1.8 2 1.8 9 0 11M7 1.5c-1.8 2-1.8 9 0 11" stroke="currentColor" strokeWidth="1.2"/>
    </svg>
  ),
  Clock: ({ size = 14, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <circle cx="7" cy="7" r="5.5" stroke="currentColor" strokeWidth="1.3"/>
      <path d="M7 3.5V7l2.5 1.5" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round"/>
    </svg>
  ),
  Building: ({ size = 14, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" fill="none" className={className}>
      <path d="M3 12.5V2.5h6v10M9 6.5h2.5V12.5M5 4.5h2M5 6.5h2M5 8.5h2M5 10.5h2"
            stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" strokeLinecap="round"/>
    </svg>
  ),
  Stop: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" className={className}>
      <rect x="3" y="3" width="8" height="8" rx="1" fill="currentColor"/>
    </svg>
  ),
  Play: ({ size = 13, className }: IconProps) => (
    <svg width={size} height={size} viewBox="0 0 14 14" className={className}>
      <path d="M4 2.5l8 4.5-8 4.5z" fill="currentColor"/>
    </svg>
  ),
};

type SortKind = "profile" | "proxy";
type SortPlacement = "before" | "after";

const listCollisionDetection: CollisionDetection = (args) => {
  const pointerHits = pointerWithin(args);
  return pointerHits.length > 0 ? pointerHits : closestCenter(args);
};

function useListSortSensors() {
  return useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
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

function dropPlacementFor(
  event: DragOverEvent | DragEndEvent,
  orderedIds: string[],
): SortPlacement {
  const translated = event.active.rect.current.translated;
  if (translated && event.over) {
    const activeCenter = translated.top + translated.height / 2;
    const overCenter = event.over.rect.top + event.over.rect.height / 2;
    return activeCenter > overCenter ? "after" : "before";
  }
  const activeIndex = orderedIds.indexOf(String(event.active.id));
  const overIndex = event.over ? orderedIds.indexOf(String(event.over.id)) : -1;
  return activeIndex >= 0 && overIndex >= 0 && activeIndex < overIndex ? "after" : "before";
}

function SortableRow({
  id,
  kind,
  disabledReason,
  className,
  rowClassName,
  dropPlacement,
  onContextMenu,
  children,
  footer,
}: {
  id: string;
  kind: SortKind;
  disabledReason?: string;
  className: string;
  rowClassName: string;
  dropPlacement: SortPlacement | null;
  onContextMenu?: (event: ReactMouseEvent<HTMLDivElement>) => void;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const disabled = !!disabledReason;
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({
    id,
    disabled,
    data: { type: "sortable-row", kind, entityId: id },
  });
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    zIndex: isDragging ? 5 : undefined,
  };
  return (
    <div
      ref={setNodeRef}
      style={style}
      className={`${className} ${isDragging ? "row-sorting" : ""} ${dropPlacement ? `row-drop-${dropPlacement}` : ""}`}
      onContextMenu={onContextMenu}
    >
      <div className={rowClassName}>
        <div className="sort-handle-cell">
          <button
            type="button"
            className="sort-handle"
            disabled={disabled}
            title={disabledReason || "Drag to reorder · Space + arrow keys also work"}
            aria-label={disabledReason || `Reorder ${kind}`}
            {...attributes}
            {...listeners}
          >
            <Icon.Grip />
          </button>
        </div>
        {children}
      </div>
      {footer}
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

function PageDropButton({
  dropId,
  direction,
  disabled,
  onClick,
  children,
}: {
  dropId: string;
  direction: "previous" | "next";
  disabled: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  const { setNodeRef, isOver } = useDroppable({
    id: dropId,
    disabled,
    data: { type: "page", direction },
  });
  return (
    <button
      ref={setNodeRef}
      className={`btn-ghost btn-sm ${isOver ? "pager-drop-active" : ""}`}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

// Reload when the backend signals an out-of-band store change — a profile or
// proxy created/edited/removed through the automation API or MCP writes
// straight to disk, so the React state never hears about it on its own.  The
// backend emits "store-changed"; without this listener the new items only show
// up after an app restart.  Bursts (e.g. MCP adding many proxies in a loop)
// are coalesced into a single reload.
function useStoreChanged(onChange: () => void) {
  const cb = useRef(onChange);
  cb.current = onChange;
  useEffect(() => {
    let disposed = false;
    let un: (() => void) | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    listen("store-changed", () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => cb.current(), 200);
    }).then((fn) => {
      if (disposed) fn();
      else un = fn;
    });
    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
      un?.();
    };
  }, []);
}

// ---- Browsers view ----

function BrowsersView() {
  const [profiles, setProfiles] = useState<ProfileMeta[]>([]);
  const [proxies, setProxies] = useState<ProxyEntry[]>([]);
  const [proxySnapshots, setProxySnapshots] = useState<Record<string, ProxyTestSnapshot>>({});
  const [search, setSearch] = useState("");
  const [folder, setFolder] = useState("all");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<ProfileForm | null>(null);
  // Value = epoch ms at which the engine was first observed running. Used
  // both as a truthy flag (any number = running) and as the anchor for the
  // ticking uptime display in the Status column.
  const [running, setRunning] = useState<Record<string, number>>({});
  const [backendActiveIds, setBackendActiveIds] = useState<Set<string>>(new Set());
  // Re-render trigger so the uptime label ticks every second without
  // re-fetching the process list (which polls every 2s).
  const [, setUptimeTick] = useState(0);
  useEffect(() => {
    if (Object.keys(running).length === 0) return;
    const h = setInterval(() => setUptimeTick((t) => t + 1), 1000);
    return () => clearInterval(h);
  }, [running]);
  const [startBusy, setStartBusy] = useState<Set<string>>(new Set());
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
  const [folderRegistry, setFolderRegistry] = useState<string[]>(() => {
    try { return JSON.parse(localStorage.getItem("shardx-folders") || "[]"); }
    catch { return []; }
  });
  const [folderModal, setFolderModal] = useState<{ profileId: string | null } | null>(null);
  const sortSensors = useListSortSensors();
  const [activeSortId, setActiveSortId] = useState<string | null>(null);
  const [sortIndicator, setSortIndicator] = useState<{ id: string; placement: SortPlacement } | null>(null);
  const [pageHover, setPageHover] = useState<"previous" | "next" | null>(null);
  const rememberFolder = (f: string) =>
    setFolderRegistry((r) => {
      const next = r.includes(f) ? r : [...r, f];
      localStorage.setItem("shardx-folders", JSON.stringify(next));
      return next;
    });
  const forgetFolder = (f: string) =>
    setFolderRegistry((r) => {
      const next = r.filter((x) => x !== f);
      localStorage.setItem("shardx-folders", JSON.stringify(next));
      return next;
    });
  const ctx = useContextMenu();

  const reload = async () => {
    try {
      setProfiles(await invoke<ProfileMeta[]>("profile_list"));
      setProxies(await invoke<ProxyEntry[]>("proxy_list"));
    } catch (e) {
      toast.err(String(e));
    }
  };
  useEffect(() => { reload(); }, []);
  // Pick up profiles/proxies created via the automation API or MCP live.
  useStoreChanged(reload);
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
      try {
        const [list, activeIds] = await Promise.all([
          invoke<{ profile_id: string; pid: number; uptime_ms: number }[]>("process_list"),
          invoke<string[]>("profile_active_ids"),
        ]);
        if (cancelled) return;
        setBackendActiveIds(new Set(activeIds));
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

  // Pagination of the (filtered) profile list.
  const PAGE_SIZE = 20;
  const [page, setPage] = useState(1);
  const pageCount = Math.max(1, Math.ceil(visible.length / PAGE_SIZE));
  // Reset to page 1 when the filter changes; clamp if the list shrank.
  useEffect(() => { setPage(1); }, [folder, search]);
  useEffect(() => { if (page > pageCount) setPage(pageCount); }, [pageCount, page]);
  const paged = useMemo(
    () => visible.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE),
    [visible, page],
  );
  useEffect(() => {
    if (!pageHover) return;
    const timer = setTimeout(() => {
      setPage((current) => pageHover === "previous"
        ? Math.max(1, current - 1)
        : Math.min(pageCount, current + 1));
      setPageHover(null);
    }, 600);
    return () => clearTimeout(timer);
  }, [pageHover, pageCount]);

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
  const selectedHasActive = selectedHasRunning || selectedHasStarting;

  // Block the Start button until `invoke("launch")` returns (success or
  // failure).  The launch includes pre-flight steps that can take real time
  // — UDP probe, geo lookup, Widevine pre-warm — and surfacing the busy
  // state for the whole window is what the user sees as "did it work?".
  // On failure we unlock immediately and toast the error.
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
      toast.err("This browser profile is already starting");
      return;
    }
    setStartBusy((s) => new Set([...s, p.id]));
    try {
      await invoke<number>("launch", { profileId: p.id });
      // Don't optimistically flip `running` here; the 2s poll above picks
      // up the new child immediately and anchors the uptime clock.
    } catch (e) {
      toast.err(String(e));
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
      toast.err("Stop the browser before deleting this profile");
      return;
    }
    if ((await confirmModal({ title: "Delete profile", message: "Delete this profile? Its user-data dir is wiped too.", danger: true })) !== true) return;
    try {
      await invoke("profile_delete", { id });
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const cloneProfile = async (id: string) => {
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      toast.err("Stop the browser before cloning this profile");
      return;
    }
    try {
      await invoke<ProfileMeta>("profile_clone", { id });
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const exportProfiles = async (ids: string[]) => {
    if (ids.length === 0) return;
    const activeIds = ids.filter((id) => running[id] || startBusy.has(id) || backendActiveIds.has(id));
    if (activeIds.length > 0) {
      toast.err(`Stop the ${activeIds.length} active browser${activeIds.length === 1 ? "" : "s"} before exporting`);
      return;
    }
    try {
      const summary = await invoke<ProfileBackupSummary>("profile_backup_export", { profileIds: ids });
      toast.ok(
        `Exported ${summary.profileCount} profile${summary.profileCount === 1 ? "" : "s"} + ${summary.cookieCount} cookie${summary.cookieCount === 1 ? "" : "s"}. Keep backup files private.`,
      );
      // Backups are written only inside the launcher's fixed portable exports
      // directory; no arbitrary destination path crosses the trust boundary.
      await invoke("open_exports_dir");
    } catch (e) { toast.err(String(e)); }
  };

  // Per-profile action menu shared by right-click and ⋮ button.
  const profileMenu = (p: ProfileMeta) => [
    { label: running[p.id] ? "Stop" : "Launch", onClick: () => startStop(p) },
    { label: "Edit", onClick: () => expand(p.id) },
    { label: "Clone", onClick: () => cloneProfile(p.id) },
    { sep: true, label: "", onClick: () => {} },
    { label: "Move to folder…", onClick: () => setFolderModal({ profileId: p.id }) },
    ...(p.folder
      ? [{ label: "Remove from folder", onClick: () => setProfileFolder(p.id, "") }]
      : []),
    { sep: true, label: "", onClick: () => {} },
    {
      label: "Export profile",
      onClick: () => exportProfiles([p.id]),
      title: "Export fingerprint and sensitive cookies as one .shardx-backup file",
    },
    { sep: true, label: "", onClick: () => {} },
    { label: "Delete", onClick: () => remove(p.id), danger: true },
  ];

  const beginRename = (p: ProfileMeta) => {
    if (running[p.id] || startBusy.has(p.id) || backendActiveIds.has(p.id)) return;
    if (expanded === p.id) {
      setExpanded(null);
      setDraft(null);
    }
    setRenaming({ id: p.id, draft: p.name });
  };

  const commitRename = async (id: string, draftName: string) => {
    const profile = profiles.find((p) => p.id === id);
    if (!profile) {
      setRenaming((current) => current?.id === id ? null : current);
      return;
    }
    if (running[id] || startBusy.has(id) || backendActiveIds.has(id)) {
      setRenaming((current) => current?.id === id ? null : current);
      toast.err("Stop the browser or wait for it to finish starting before renaming");
      return;
    }

    const name = draftName.trim();
    if (name === profile.name) {
      setRenaming((current) => current?.id === id ? null : current);
      return;
    }

    try {
      await invoke("profile_rename", { id, name });
      setRenaming((current) => current?.id === id ? null : current);
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
      toast.err("Stop the browser before moving this profile");
      return;
    }
    if (p && p.folder === f) {
      const who = p.name || id.slice(0, 8);
      toast.info(f ? `“${who}” is already in “${f}”` : `“${who}” isn’t in any folder`);
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
      toast.err(`Stop the ${runningCount} running browser${runningCount === 1 ? "" : "s"} in this folder first`);
      return;
    }
    // Three outcomes: delete profiles, unfile, cancel.
    const choice = await confirmModal({
      title: `Delete folder “${f}”`,
      message:
        count > 0
          ? `This folder has ${count} profile${count === 1 ? "" : "s"}. ` +
            `Delete them too, or keep them (they move to “All”)?`
          : `Delete the empty folder “${f}”?`,
      buttons:
        count > 0
          ? [
              { label: "Cancel", value: "cancel" },
              { label: "Keep profiles", value: "keep" },
              { label: "Delete profiles", value: "delete", danger: true },
            ]
          : [
              { label: "Cancel", value: "cancel" },
              { label: "Delete", value: "keep", danger: true },
            ],
    });
    if (choice == null || choice === "cancel") return;
    const alsoDelete = choice === "delete";
    try {
      const n = await invoke<number>("folder_delete", { folder: f, deleteProfiles: alsoDelete });
      // The folder lives in two places: profile tags (cleared by folder_delete)
      // and the localStorage registry of empty folders.  Drop it from the
      // registry too, otherwise the tab lingers after every profile is gone.
      forgetFolder(f);
      if (folder === f) setFolder("all");
      reload();
      toast.ok(
        alsoDelete
          ? `Deleted folder “${f}” + ${n} profile${n === 1 ? "" : "s"}`
          : `Removed folder “${f}” (${n} profile${n === 1 ? "" : "s"} kept)`,
      );
    } catch (e) { toast.err(String(e)); }
  };

  const bulkLaunch = async () => {
    const ids = [...selected];
    if (ids.length === 0) return;
    const runningIds = ids.filter((id) => !!running[id]);
    if (runningIds.length > 0) {
      toast.err(`Stop the ${runningIds.length} selected running browser${runningIds.length === 1 ? "" : "s"} before launching`);
      return;
    }
    const startingIds = ids.filter((id) => startBusy.has(id) || (backendActiveIds.has(id) && !running[id]));
    if (startingIds.length > 0) {
      toast.err("Wait for the selected browser to finish starting");
      return;
    }

    setStartBusy((state) => new Set([...state, ...ids]));
    try {
      for (const id of ids) {
        try {
          await invoke<number>("launch", { profileId: id });
        } catch (e) {
          toast.err(`Failed to launch ${id.slice(0, 8)}: ${e}`);
        }
      }
      setSelected(new Set());
    } finally {
      setStartBusy((state) => {
        const next = new Set(state);
        for (const id of ids) next.delete(id);
        return next;
      });
    }
  };

  const bulkStop = async () => {
    for (const id of selected) {
      try { await invoke<boolean>("process_kill", { profileId: id }); } catch {}
    }
    setSelected(new Set());
  };

  const bulkDelete = async () => {
    const ids = [...selected];
    if (ids.length === 0) return;
    const activeIds = ids.filter((id) => !!running[id] || startBusy.has(id) || backendActiveIds.has(id));
    if (activeIds.length > 0) {
      toast.err(`Stop the ${activeIds.length} selected active browser${activeIds.length === 1 ? "" : "s"} first`);
      return;
    }
    if ((await confirmModal({ title: "Delete profiles", message: `Delete ${ids.length} profile${ids.length === 1 ? "" : "s"}? This wipes their user-data dirs too.`, danger: true })) !== true) return;
    for (const id of ids) {
      try { await invoke("profile_delete", { id }); } catch (e) { toast.err(String(e)); }
    }
    setSelected(new Set());
    reload();
    toast.ok(`Deleted ${ids.length}`);
  };

  const bulkExport = async () => {
    const ids = [...selected];
    await exportProfiles(ids);
  };

  const bulkImport = async () => {
    try {
      const files = await pickProfileBackupFiles();
      if (!files) return;
      const summary = await invoke<ProfileBackupSummary>("profile_backup_import", { files });
      await reload();
      toast.ok(
        `Imported ${summary.profileCount} profile${summary.profileCount === 1 ? "" : "s"} + ${summary.cookieCount} cookie${summary.cookieCount === 1 ? "" : "s"}`,
      );
    } catch (e) {
      toast.err("Import failed: " + String(e));
    }
  };

  const expand = async (id: string) => {
    if (running[id]) {
      toast.err("Stop the browser before editing this profile");
      return;
    }
    setRenaming(null);
    if (expanded === id) { setExpanded(null); setDraft(null); return; }
    const stored = await invoke<any>("profile_get", { id });
    setDraft(fromStored(stored));
    setExpanded(id);
  };

  const newProfile = async () => {
    setRenaming(null);
    setDraft(defaultForm());
    setExpanded("__new__");
  };

  const saveDraft = async () => {
    if (!draft) return;
    const nameError = profileNameError(draft.name);
    if (nameError) {
      toast.err(nameError);
      return;
    }
    try {
      const fp = fingerprints.find((g) => g.id === draft.gpu_preset_id) ?? null;
      // Preserve the durable config when editing an existing profile. In
      // particular, its screen/window block was clamped to this display when
      // the profile was created and must not be replaced by donor dimensions
      // merely because NAME (or another unrelated field) changed.
      const existing = draft.id
        ? await invoke<any>("profile_get", { id: draft.id })
        : null;
      const saved = await invoke<ProfileMeta>("profile_save", {
        payload: toStored(draft, fp, existing),
      });
      await invoke("profile_bind_proxy", { profileId: saved.id, proxyId: draft.proxy_id });
      // A profile created while a folder tab is active should land in that
      // folder (otherwise it pops into "All" and the user has to drag it
      // back themselves).  `__new__` test scopes this to creations only —
      // edits preserve whatever folder the profile already had.
      if (!draft.id && folder && folder !== "all") {
        try { await invoke("profile_set_folder", { id: saved.id, folder }); }
        catch (e) { console.warn("auto-assign folder failed:", e); }
      }
      setExpanded(null);
      setDraft(null);
      reload();
      toast.ok(draft.id ? "Profile saved" : `Created "${saved.name}"`);
    } catch (e) { toast.err(String(e)); }
  };

  const toggleSel = (id: string) => {
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id); else n.add(id);
      return n;
    });
  };

  const resetProfileDrag = () => {
    setActiveSortId(null);
    setSortIndicator(null);
    setPageHover(null);
  };

  const handleProfileDragOver = (event: DragOverEvent) => {
    const over = event.over;
    if (!over) {
      setSortIndicator(null);
      setPageHover(null);
      return;
    }
    const overType = over.data.current?.type;
    if (overType === "page") {
      setSortIndicator(null);
      setPageHover(over.data.current?.direction === "previous" ? "previous" : "next");
      return;
    }
    setPageHover(null);
    if (overType !== "sortable-row" || over.data.current?.kind !== "profile") {
      setSortIndicator(null);
      return;
    }
    const active = profiles.find((profile) => profile.id === String(event.active.id));
    const target = profiles.find((profile) => profile.id === String(over.id));
    if (!active || !target || active.id === target.id) {
      setSortIndicator(null);
      return;
    }
    setSortIndicator({
      id: target.id,
      placement: dropPlacementFor(event, paged.map((profile) => profile.id)),
    });
  };

  const handleProfileDragEnd = async (event: DragEndEvent) => {
    const activeId = String(event.active.id);
    const over = event.over;
    resetProfileDrag();
    if (!over || activeId === String(over.id)) return;

    if (over.data.current?.type === "profile-folder") {
      await setProfileFolder(activeId, String(over.data.current.folder ?? ""));
      return;
    }
    if (over.data.current?.type !== "sortable-row" || over.data.current?.kind !== "profile") return;

    const anchorId = String(over.id);
    const moving = profiles.find((profile) => profile.id === activeId);
    const anchor = profiles.find((profile) => profile.id === anchorId);
    if (!moving || !anchor) return;
    const placement = dropPlacementFor(event, paged.map((profile) => profile.id));
    const previous = profiles;
    setProfiles(moveByAnchor(profiles, activeId, anchorId, placement));
    try {
      await invoke("profile_move_order", { id: activeId, anchorId, placement });
    } catch (error) {
      setProfiles(previous);
      toast.err(`Could not save profile order: ${String(error)}`);
    }
  };

  return (
    <DndContext
      sensors={sortSensors}
      collisionDetection={listCollisionDetection}
      onDragStart={(event) => setActiveSortId(String(event.active.id))}
      onDragOver={handleProfileDragOver}
      onDragCancel={resetProfileDrag}
      onDragEnd={handleProfileDragEnd}
    >
    <section className="page workspace-page">
      <Topbar crumbs={["Workspace", "Browsers"]} search={search} onSearch={setSearch} />

      <div className="metric-strip">
        <Metric label="Profiles" value={String(profiles.length)} accent />
        <Metric label="Running" value={String(runningCount)} pulse={runningCount > 0} />
        <Metric label="Proxies" value={String(proxies.length)} />
        <Metric label="Fingerprints" value={String(fingerprints.length)} />
      </div>

      <div className="page-title">
        <div className="title-with-tabs">
          <h1>Browsers</h1>
          <div className="folder-tabs" ref={folderTabsRef}>
            <FolderDropTab
              dropId="profile-folder:all"
              folder=""
              className={`folder-tab ${folder === "all" ? "active" : ""}`}
              onClick={() => setFolder("all")}
            >
              All<span className="tab-count">{profiles.length}</span>
            </FolderDropTab>
            {folders.map((f) => (
              <FolderDropTab
                key={f}
                dropId={`profile-folder:${f}`}
                folder={f}
                className={`folder-tab ${folder === f ? "active" : ""}`}
                onClick={() => setFolder(f)}
                title="Right-click for folder actions · drop profiles to move them"
                onContextMenu={(e) =>
                  ctx.open(e, [
                    { label: "Delete folder…", onClick: () => deleteFolder(f), danger: true },
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
              title="Create a new folder"
              onClick={() => setFolderModal({ profileId: null })}
            >
              +
            </button>
          </div>
        </div>
        <div className="page-actions">
          {selected.size > 0 && (
            <div className="bulk-bar bulk-bar-floating">
              <span>{selected.size} selected</span>
              <button
                className="btn-ghost btn-sm"
                onClick={bulkLaunch}
                disabled={selectedHasActive}
                title={selectedHasRunning ? "Stop selected running browsers before launching" : selectedHasStarting ? "Wait for selected browsers to finish starting" : "Launch selected profiles"}
              ><Icon.Play /> Launch</button>
              <button className="btn-ghost btn-sm" onClick={bulkStop}><Icon.Stop /> Stop</button>
              <button
                className="btn-ghost btn-sm"
                onClick={bulkExport}
                disabled={selectedHasActive}
                title={selectedHasActive ? "Stop selected active browsers before exporting" : "Export selected profiles with their sensitive cookies"}
              ><Icon.Upload /> Export</button>
              <button
                className="btn-ghost btn-sm"
                onClick={bulkDelete}
                disabled={selectedHasActive}
                title={selectedHasActive ? "Stop selected active browsers before deleting" : "Delete selected profiles"}
              ><Icon.Trash /> Delete</button>
            </div>
          )}
          <button className="btn-ghost profile-page-action" onClick={bulkImport} title="Import one or more .shardx-backup files"><Icon.Download /> Import profile</button>
          <button className="btn-primary profile-page-action" onClick={newProfile}>+ New profile</button>
        </div>
      </div>
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

      <div className="rows">
        <div className="rows-head t-cols">
          <div className="sort-head" title="Drag rows to reorder"><Icon.Grip /></div>
          <div></div>
          <div>
            <input
              type="checkbox"
              title="Select all on this page"
              // Header checkbox toggles only visible page rows; other pages preserved.
              checked={paged.length > 0 && paged.every((p) => selected.has(p.id))}
              ref={(el) => {
                if (!el) return;
                const anySel = paged.some((p) => selected.has(p.id));
                const allSel = paged.length > 0 && paged.every((p) => selected.has(p.id));
                el.indeterminate = anySel && !allSel;
              }}
              onChange={(e) => {
                setSelected((prev) => {
                  const next = new Set(prev);
                  if (e.target.checked) {
                    for (const p of paged) next.add(p.id);
                  } else {
                    for (const p of paged) next.delete(p.id);
                  }
                  return next;
                });
              }}
            />
          </div>
          <div>Name</div><div>Status</div><div>Proxy</div><div>Notes</div><div className="head-time">Time</div><div className="head-lastrun">Last run</div><div className="head-actions">ACTIONS</div>
        </div>
        {expanded === "__new__" && draft && (
          <div className="row-wrap row-expanded row-new">
            <InlineEditor
              draft={draft}
              setDraft={setDraft}
              proxies={proxies}
              fingerprints={fingerprints}
              onSave={saveDraft}
              onCancel={() => { setExpanded(null); setDraft(null); }}
            />
          </div>
        )}
        <SortableContext items={paged.map((profile) => profile.id)} strategy={verticalListSortingStrategy}>
        {paged.map((p) => {
          const px = p.proxy_id ? proxyMap[p.proxy_id] : null;
          const proxySnapshot = px ? proxySnapshots[px.id] : undefined;
          const proxyCountry = (proxySnapshot?.country_code || px?.country || "").trim().toUpperCase();
          const proxyLocation = compactProxyLocation(
            proxySnapshot?.city,
            proxySnapshot?.region,
            proxySnapshot?.country,
          );
          const hostLooksLikeIp = !!px && (/^(?:\d{1,3}\.){3}\d{1,3}$/.test(px.host) || px.host.includes(":"));
          // Before the first test, an IP-literal endpoint is a useful fallback.
          // Once a test exists (including a failed one), never relabel the
          // endpoint as the detected exit IP.
          const proxyIp = (proxySnapshot?.ip || (!proxySnapshot && hostLooksLikeIp ? px?.host : "") || "").trim();
          const proxyDetailLocation = proxyLocation || (!proxySnapshot && proxyIp ? proxyCountry : "");
          const proxyDetailText = [proxyDetailLocation, proxyIp].filter(Boolean).join(" · ");
          const isRunning = !!running[p.id];
          const isStarting = !isRunning && (startBusy.has(p.id) || backendActiveIds.has(p.id));
          const isActive = isRunning || isStarting;
          const isExpanded = expanded === p.id;
          const isSel = selected.has(p.id);
          return (
            <SortableRow
              key={p.id}
              id={p.id}
              kind="profile"
              className={`row-wrap ${isRunning ? "row-running" : ""} ${isExpanded ? "row-expanded" : ""}`}
              rowClassName="row t-cols browser-data-row"
              dropPlacement={sortIndicator?.id === p.id ? sortIndicator.placement : null}
              disabledReason={isActive ? "Stop the browser before reordering this profile" : search.trim() ? "Clear search to reorder profiles" : isExpanded ? "Close the editor before reordering" : undefined}
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
                  setDraft={setDraft}
                  proxies={proxies}
                  fingerprints={fingerprints}
                  onSave={saveDraft}
                  onCancel={() => { setExpanded(null); setDraft(null); }}
                />
              ) : undefined}
            >
                <div className="cell-strip">
                  <span className={`shard ${isRunning ? "shard-on" : "shard-off"}`} />
                </div>
                <div>
                  <input type="checkbox" checked={isSel} onChange={() => toggleSel(p.id)} />
                </div>
                <div
                  className={`cell-name ${isActive ? "cell-locked" : ""}`}
                  onClick={isActive || renaming?.id === p.id ? undefined : () => beginRename(p)}
                  title={isActive ? "Stop the browser or wait for it to finish starting before renaming" : "Click to rename"}
                >
                  {renaming?.id === p.id ? (
                    <input
                      autoFocus
                      className="inline-rename profile-inline-rename"
                      value={renaming.draft}
                      onClick={(e) => e.stopPropagation()}
                      onChange={(e) => setRenaming({ id: p.id, draft: e.target.value })}
                      onBlur={(e) => commitRename(p.id, e.currentTarget.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") e.currentTarget.blur();
                        else if (e.key === "Escape") setRenaming(null);
                      }}
                      aria-label={`Rename ${p.name}`}
                    />
                  ) : (
                    <div className="name-main">{p.name}</div>
                  )}
                  <div className="name-sub">{p.id.slice(0, 8)}</div>
                </div>
                <div>
                  <span className={`pill-status ${isRunning ? "ps-on" : "ps-off"}`}>
                    <i className="dot" />
                    {isRunning ? "Running" : "Idle"}
                  </span>
                </div>
                <div
                  className={`cell-proxy cell-click ${isActive ? "cell-locked" : ""}`}
                  onClick={isActive ? undefined : () => setQuickEdit({ kind: "proxy", profile: p })}
                  title={isActive ? "Stop the browser or wait for it to finish starting before changing proxy" : "Change proxy"}
                >
                  {px ? (
                    <div className="proxy-cell">
                      <div className="proxy-main">
                        <span className={`badge badge-${px.kind}`}>{px.kind}</span>
                        {proxyCountry && (
                          <>
                            <CountryFlag cc={proxyCountry} />
                            <span className="flag">{proxyCountry}</span>
                          </>
                        )}
                      </div>
                      <div
                        className={`proxy-detail ${proxyDetailText ? "" : "muted"}`}
                        title={proxyDetailText || "No test data"}
                      >
                        <span className="proxy-detail-text">{proxyDetailText || "No test data"}</span>
                      </div>
                    </div>
                  ) : <span className="muted small">— direct —</span>}
                </div>
                <div
                  className={`cell-notes cell-click ${isActive ? "cell-locked" : ""}`}
                  title={isActive ? "Stop the browser or wait for it to finish starting before editing notes" : p.notes || "Click to edit notes"}
                  onClick={isActive ? undefined : () => setQuickEdit({ kind: "notes", profile: p })}
                >
                  {p.notes || <span className="muted">—</span>}
                </div>
                <div className="cell-time">
                  <span className={`small ${isRunning ? "" : "muted"}`}>
                    {(() => {
                      const live = isRunning ? Date.now() - running[p.id] : 0;
                      const total = p.total_runtime_ms + live;
                      return total > 0 ? fmtUptime(total) : "—";
                    })()}
                  </span>
                </div>
                <div className="cell-lastrun"><span className="muted small">{p.last_launched_at ? fmtTs(p.last_launched_at) : "never"}</span></div>
                <div className="row-actions">
                  <button
                    className={`btn-launch ${isRunning ? "btn-launch-stop" : ""}`}
                    onClick={() => startStop(p)}
                    disabled={isStarting}
                    title={
                      isRunning
                        ? "Stop browser"
                        : isStarting
                          ? "Starting browser (UDP probe + geo + spawn)…"
                          : "Start browser"
                    }
                    aria-label={isRunning ? "Stop browser" : isStarting ? "Starting browser" : "Start browser"}
                    aria-busy={isStarting}
                  >
                    <span className={`btn-launch-ico ${isStarting ? "spin" : ""}`}>
                      {isRunning ? <Icon.Stop /> : isStarting ? <Icon.Loader /> : <Icon.Play />}
                    </span>
                  </button>
                  <button className="icon-btn" onClick={() => expand(p.id)} disabled={isActive} title={isActive ? "Stop browser before editing" : "Edit"}><Icon.Edit /></button>
                  <button className="icon-btn" onClick={() => cloneProfile(p.id)} disabled={isActive} title={isActive ? "Stop browser before cloning" : "Clone"}><Icon.Clone /></button>
                  <button className="icon-btn danger" onClick={() => remove(p.id)} disabled={isActive} title={isActive ? "Stop browser before deleting" : "Delete"}><Icon.Trash /></button>
                  <button
                    className="icon-btn"
                    onClick={(e) => { e.stopPropagation(); ctx.open(e, profileMenu(p)); }}
                    disabled={isActive}
                    title={isActive ? "Stop browser before using more actions" : "More actions"}
                  ><Icon.More /></button>
                </div>
            </SortableRow>
          );
        })}
        </SortableContext>
        {visible.length === 0 && !expanded && (
          <div className="empty-rich">
            <div className="empty-shard"><ShardLogo /></div>
            <h3>No profiles yet</h3>
            <p>Create a browser profile to get started.</p>
            <div className="empty-cta">
              <button className="btn-primary profile-page-action" onClick={newProfile}>+ New profile</button>
            </div>
          </div>
        )}
      </div>
      {pageCount > 1 && (
        <div className="pager">
          <PageDropButton
            dropId="profile-page-previous"
            direction="previous"
            disabled={page <= 1}
            onClick={() => setPage((p) => Math.max(1, p - 1))}
          >‹ Prev</PageDropButton>
          <span className="pager-info">Page {page} of {pageCount} · {visible.length} profiles</span>
          <PageDropButton
            dropId="profile-page-next"
            direction="next"
            disabled={page >= pageCount}
            onClick={() => setPage((p) => Math.min(pageCount, p + 1))}
          >Next ›</PageDropButton>
        </div>
      )}
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
    <DragOverlay>
      {activeSortId && (
        <div className="sort-overlay">
          <Icon.Grip />
          {profiles.find((profile) => profile.id === activeSortId)?.name || activeSortId.slice(0, 8)}
        </div>
      )}
    </DragOverlay>
    </DndContext>
  );
}

/// Keep browser-table proxy metadata compact. If city and region overlap,
/// retain the more specific value ("Singapore" + "Central Singapore" becomes
/// "Central Singapore"). Preserve both values when they are distinct, such as
/// "Los Angeles, California".
function compactProxyLocation(cityValue?: string, regionValue?: string, countryValue?: string) {
  const city = (cityValue || "").trim();
  const region = (regionValue || "").trim();
  const country = (countryValue || "").trim();
  if (!city) return region || country;
  if (!region) return city;
  const cityKey = city.toLowerCase();
  const regionKey = region.toLowerCase();
  if (regionKey.includes(cityKey)) return region;
  if (cityKey.includes(regionKey)) return city;
  return `${city}, ${region}`;
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
      toast.ok("Proxy updated");
      onSaved();
    } catch (e) { toast.err(String(e)); }
  };

  const saveNotes = async () => {
    try {
      // Round-trip the whole profile JSON so the user's other fields stay intact.
      const stored = await invoke<any>("profile_get", { id: profile.id });
      stored.notes = notes;
      await invoke<ProfileMeta>("profile_save", { payload: stored });
      toast.ok("Notes saved");
      onSaved();
    } catch (e) { toast.err(String(e)); }
  };

  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog">
        <header className="dialog-head">
          <h2><ShardMini /> {kind === "proxy" ? "Bind proxy" : "Edit notes"} — {profile.name}</h2>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </header>
        <div className="dialog-body">
          {kind === "proxy" ? (
            <label>
              <span className="lbl">Proxy</span>
              <select value={proxyId ?? ""} onChange={(e) => setProxyId(e.target.value || null)}>
                <option value="">— direct connection —</option>
                {proxies.map((px) => (
                  <option key={px.id} value={px.id}>
                    {proxyBindingLabel(px, proxySnapshots[px.id])}
                  </option>
                ))}
              </select>
            </label>
          ) : (
            <label>
              <span className="lbl">Notes</span>
              <textarea rows={6} value={notes} onChange={(e) => setNotes(e.target.value)} autoFocus />
            </label>
          )}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>Cancel</button>
          <button className="btn-primary" onClick={kind === "proxy" ? saveProxy : saveNotes}>
            <ShardMini /> Save
          </button>
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
  draft, setDraft, proxies, fingerprints, onSave, onCancel,
}: {
  draft: ProfileForm;
  setDraft: (f: ProfileForm) => void;
  proxies: ProxyEntry[];
  fingerprints: FingerprintEntry[];
  onSave: () => void;
  onCancel: () => void;
}) {
  const f = draft;
  const nameError = profileNameError(f.name);
  const u = <K extends keyof ProfileForm>(k: K, v: ProfileForm[K]) => setDraft({ ...f, [k]: v });
  const draftRef = useRef(f);
  draftRef.current = f;
  const gpuPickRequest = useRef(0);
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
  const setGpu = async (id: string) => {
    const fp = fingerprints.find((x) => x.id === id);
    if (!fp) return;
    const nav = fp.payload?.navigator ?? {};
    const request = ++gpuPickRequest.current;
    let picks: PresetEnrichPicks;
    try {
      picks = await invoke<PresetEnrichPicks>("enrich_picks_for_preset", { presetId: id });
    } catch (e) {
      toast.err(`Unable to load hardware configurations: ${String(e)}`);
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
      toast.err("Backend returned an invalid hardware configuration");
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

  // Snap unknown / empty gpu_preset_id to a random GPU of the active OS.
  useEffect(() => {
    if (fingerprints.length === 0) return;
    const exists = fingerprints.some((g) => g.id === f.gpu_preset_id);
    if (!exists) {
      const pool = gpusForOs.length > 0 ? gpusForOs : fingerprints;
      const pick = pool[Math.floor(Math.random() * pool.length)];
      if (pick) setGpu(pick.id);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fingerprints, osFilter, f.gpu_preset_id]);

  const pickOs = (os: OsPlatform) => {
    setOsFilter(os);
    // Switch GPU to first of new OS if current doesn't match.
    if (currentFp && currentFp.platform !== os) {
      const first = fingerprints.find((g) => g.platform === os);
      if (first) setGpu(first.id);
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
    <div className="inline-editor">
      <div className="ie-stripe" />
      <div className="ie-grid">
        {/* ----- col 1: identity + hardware ----- */}
        <div className="ie-section">
          <div className="ie-section-title">Identity</div>
          <Field
            label="Profile name"
            value={f.name}
            onChange={(v) => u("name", v)}
            placeholder="e.g. shop-pl-1"
            error={nameError}
          />

          <label>
            <span className="lbl">Operating system</span>
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
            <span className="lbl">GPU / device (from Fingerprint Library)</span>
            <CSSelect
              value={f.gpu_preset_id}
              onChange={(v) => setGpu(v)}
              placeholder={`— no ${osFilter} fingerprints in library —`}
              options={gpusForOs.map((g) => ({ value: g.id, label: g.label }))}
            />
          </label>

          <Field label="User-Agent" value={f.user_agent} onChange={(v) => u("user_agent", v)} mono />

          <div className="form-row">
            <SelectField
              label="Logical CPUs"
              value={f.hardware_concurrency}
              onChange={setCpu}
              options={cpuOptions}
            />
            <SelectField
              label="Memory (GB)"
              value={f.device_memory}
              onChange={setMemory}
              options={memoryOptions}
            />
          </div>

          <label>
            <span className="lbl">Proxy</span>
            <CSSelect
              value={f.proxy_id ?? ""}
              onChange={(v) => u("proxy_id", v ? v : null)}
              options={[
                { value: "", label: "— direct connection —" },
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
          <div className="ie-section-title">Locale</div>
          <div className="form-row">
            <label>
              <span className="lbl">Timezone</span>
              <CSSelect
                value={f.timezone}
                onChange={(v) => u("timezone", v)}
                options={TIMEZONES.map((tz) => ({
                  value: tz,
                  label: tz === AUTO_TZ ? "Auto (from proxy geo)" : tz,
                }))}
              />
            </label>
            <label>
              <span className="lbl">Language</span>
              <CSSelect
                value={f.language}
                onChange={(v) => u("language", v)}
                options={LOCALES.map((l) => ({ value: l.code, label: l.label }))}
              />
            </label>
          </div>

          <div className="ie-section-title" style={{ marginTop: 6 }}>Noise</div>
          <div className="noise-grid noise-grid-3">
            <Pair label="Canvas"        value={f.noise_canvas}        on={(v) => u("noise_canvas", v)} />
            <Pair label="WebGL"         value={f.noise_webgl}         on={(v) => u("noise_webgl", v)} />
            <Pair label="Audio"         value={f.noise_audio}         on={(v) => u("noise_audio", v)} />
            <Pair label="Client rects"  value={f.noise_client_rects}  on={(v) => u("noise_client_rects", v)} />
            <Pair label="Sensors"       value={f.noise_sensors}       on={(v) => u("noise_sensors", v)} />
            <Pair label="Fonts"         value={f.noise_fonts}         on={(v) => u("noise_fonts", v)} onText="Noise" />
          </div>

          <PortList
            label="Ports to block"
            value={f.blocked_ports}
            onChange={(v) => u("blocked_ports", v)}
          />
        </div>

        {/* ----- col 3: privacy + media + notes ----- */}
        <div className="ie-section">
          <div className="ie-section-title">Privacy</div>
          <div className="form-row">
            <label>
              <span className="lbl">WebRTC</span>
              <CSSelect
                value={f.webrtc}
                onChange={(v) => u("webrtc", v as WebRtcMode)}
                options={[
                  { value: "auto", label: "Auto" },
                  { value: "tcp_only", label: "TCP only" },
                  { value: "block", label: "Block" },
                ]}
              />
            </label>
            <label>
              <span className="lbl">Do Not Track</span>
              <CSSelect
                value={f.do_not_track ? "1" : "0"}
                onChange={(v) => u("do_not_track", v === "1")}
                options={[
                  { value: "0", label: "Off" },
                  { value: "1", label: "On (send DNT: 1)" },
                ]}
              />
            </label>
          </div>

          <label>
            <span className="lbl">Geolocation</span>
            <div className="seg seg-2">
              {(["auto", "manual"] as GeoMode[]).map((m) => (
                <button key={m} className={`seg-btn ${f.geo_mode === m ? "active" : ""}`} onClick={() => u("geo_mode", m)}>
                  {m === "auto" ? "Auto (from proxy)" : "Manual coords"}
                </button>
              ))}
            </div>
          </label>
          {f.geo_mode === "manual" && (
            <div className="form-row form-row-3">
              <NumField label="Latitude" value={f.geo_lat} onChange={(v) => u("geo_lat", v)} step={0.0001} />
              <NumField label="Longitude" value={f.geo_lng} onChange={(v) => u("geo_lng", v)} step={0.0001} />
              <NumField label="Accuracy m" value={f.geo_accuracy} onChange={(v) => u("geo_accuracy", v)} />
            </div>
          )}

          <div className="ie-section-title" style={{ marginTop: 10 }}>Media devices</div>
          <div className="form-row form-row-3">
            <SelectField label="Mic in" value={f.media_audio_in} onChange={(v) => u("media_audio_in", v)} options={MEDIA_COUNT_OPTIONS} />
            <SelectField label="Speakers" value={f.media_audio_out} onChange={(v) => u("media_audio_out", v)} options={MEDIA_COUNT_OPTIONS} />
            <SelectField label="Webcam" value={f.media_video_in} onChange={(v) => u("media_video_in", v)} options={MEDIA_COUNT_OPTIONS} />
          </div>

          <label>
            <span className="lbl">Notes</span>
            <textarea rows={2} value={f.notes} onChange={(e) => u("notes", e.target.value)} placeholder="Free-form notes…" />
          </label>
        </div>
      </div>
      <div className="ie-foot">
        <button className="btn-ghost" onClick={onCancel}>Cancel</button>
        <button className="btn-primary" onClick={onSave} disabled={nameError != null} title={nameError ?? undefined}>
          <ShardMini /> {f.id ? "Save changes" : "Create profile"}
        </button>
      </div>
    </div>
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
  label, value, on, blockLabel, onText,
}: {
  label: string;
  value: NoiseMode;
  on: (v: NoiseMode) => void;
  /// Allow/Block labels instead of Real/Auto (used by Ports).
  blockLabel?: boolean;
  /// Custom "on" label (default "Auto noise"; Fonts passes "Noise").
  onText?: string;
}) {
  const opts: NoiseMode[] = ["real", "auto"];
  const labelFor = (o: NoiseMode) =>
    blockLabel
      ? (o === "real" ? "Allow" : "Block")
      : (o === "real" ? "Real" : (onText ?? "Auto noise"));
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
            <button type="button" className="port-chip-x" onClick={() => remove(p)} title="Remove">✕</button>
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
          placeholder={value.length === 0 ? "e.g. 3389, 5900, 8080" : "add port…"}
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

// ---- topbar + metrics ----

function Topbar({ crumbs, search, onSearch }: { crumbs: string[]; search: string; onSearch: (v: string) => void }) {
  return (
    <div className="topbar">
      <div className="crumbs">
        {crumbs.map((c, i) => (
          <span key={i}>
            {i > 0 && <span className="sep">›</span>}
            <span className={i === crumbs.length - 1 ? "crumb-now" : ""}>{c}</span>
          </span>
        ))}
      </div>
      <div className="search">
        <span className="search-icon">⌕</span>
        <input placeholder="Search…   ⌘K" value={search} onChange={(e) => onSearch(e.target.value)} />
      </div>
    </div>
  );
}

function Metric({ label, value, accent, pulse }: { label: string; value: string; accent?: boolean; pulse?: boolean }) {
  return (
    <div className={`metric ${accent ? "metric-accent" : ""}`}>
      <div className="m-k">{label}</div>
      <div className={`m-v ${pulse ? "m-v-pulse" : ""}`}>{value}</div>
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

function ProxiesView() {
  const [proxies, setProxies] = useState<ProxyEntry[]>([]);
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
  const proxySortSensors = useListSortSensors();
  const [activeProxySortId, setActiveProxySortId] = useState<string | null>(null);
  const [proxySortIndicator, setProxySortIndicator] = useState<{ id: string; placement: SortPlacement } | null>(null);
  const [proxyPageHover, setProxyPageHover] = useState<"previous" | "next" | null>(null);
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
        p.username.toLowerCase().includes(q) ||
        ip.includes(q) ||
        city.includes(q) ||
        isp.includes(q)
      );
    });
  }, [proxies, snapshots, search]);

  // Pagination over the filtered list.
  const PROXY_PAGE_SIZE = 20;
  const [proxyPage, setProxyPage] = useState(1);
  const proxyPageCount = Math.max(1, Math.ceil(filteredProxies.length / PROXY_PAGE_SIZE));
  useEffect(() => {
    if (proxyPage > proxyPageCount) setProxyPage(proxyPageCount);
  }, [proxyPageCount, proxyPage]);
  // Reset to page 1 when the search narrows the list to fewer pages.
  useEffect(() => { setProxyPage(1); }, [search]);
  const pagedProxies = useMemo(
    () => filteredProxies.slice((proxyPage - 1) * PROXY_PAGE_SIZE, proxyPage * PROXY_PAGE_SIZE),
    [filteredProxies, proxyPage],
  );
  useEffect(() => {
    if (!proxyPageHover) return;
    const timer = setTimeout(() => {
      setProxyPage((current) => proxyPageHover === "previous"
        ? Math.max(1, current - 1)
        : Math.min(proxyPageCount, current + 1));
      setProxyPageHover(null);
    }, 600);
    return () => clearTimeout(timer);
  }, [proxyPageHover, proxyPageCount]);

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
      toast.err("Stop the running browser using this proxy before renaming it");
      return;
    }
    const newName = renaming.draft.trim();
    if (newName === entry.name) { setRenaming(null); return; }
    try {
      await invoke("proxy_save", { entry: { ...entry, name: newName } });
      setRenaming(null);
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const reload = async () => {
    try {
      setProxies(await invoke<ProxyEntry[]>("proxy_list"));
      // Profile list powers the per-proxy bound-count column.
      setProfiles(await invoke<ProfileMeta[]>("profile_list"));
    } catch (e) { toast.err(String(e)); }
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
      toast.err("Stop the running browser using this proxy before deleting it");
      return;
    }
    if ((await confirmModal({ title: "Delete proxy", message: "Delete this proxy?", danger: true })) !== true) return;
    try { await invoke("proxy_delete", { id }); reload(); toast.ok("Proxy deleted"); }
    catch (e) { toast.err(String(e)); }
  };

  // The backend keeps at most five tests in flight. Every queued proxy receives
  // its own complete five-second test window once a worker becomes available.
  const bulkTest = async () => {
    const ids = [...proxySel];
    if (ids.length === 0) return;
    const targets = proxies.filter((p) => proxySel.has(p.id));
    toast.info(`Testing ${ids.length} prox${ids.length === 1 ? "y" : "ies"}…`);
    setBusy((state) => {
      const next = { ...state };
      for (const target of targets) next[target.id] = true;
      return next;
    });
    try {
      const results = await invoke<ProxyBatchTestResult[]>("proxy_full_test_batch", { entries: targets });
      const tested: Record<string, ProxyTestSnapshot> = {};
      let passed = 0;
      let failed = 0;
      let incomplete = 0;
      for (const result of results) {
        const target = targets[result.index];
        if (!target) continue;
        if (!result.snapshot) {
          incomplete += 1;
          continue;
        }
        tested[target.id] = result.snapshot;
        if (result.snapshot.tcp_ms != null) passed += 1;
        else failed += 1;
      }
      setSnapshots((state) => ({ ...state, ...tested }));
      await reload();
      const summary = `Bulk test done: ${passed} passed, ${failed} failed${incomplete > 0 ? `, ${incomplete} incomplete` : ""}`;
      if (failed === 0 && incomplete === 0) toast.ok(summary);
      else toast.info(summary);
    } catch (e) {
      toast.err(`Bulk test failed: ${e}`);
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
      toast.err(`Stop browsers using the ${lockedIds.length} selected locked prox${lockedIds.length === 1 ? "y" : "ies"} before deleting`);
      return;
    }
    if ((await confirmModal({ title: "Delete proxies", message: `Delete ${ids.length} prox${ids.length === 1 ? "y" : "ies"}?`, danger: true })) !== true) return;
    for (const id of ids) {
      try { await invoke("proxy_delete", { id }); } catch (e) { toast.err(String(e)); }
    }
    setProxySel(new Set());
    reload();
    toast.ok(`Deleted ${ids.length}`);
  };

  // Export in bulk-import format so round-trip preserves country tag.
  const bulkExport = () => {
    const targets = proxies.filter((p) => proxySel.has(p.id));
    if (targets.length === 0) return;
    const lines = targets.map((p) => {
      const auth = p.username || p.password ? `${p.username}:${p.password}@` : "";
      const base = `${p.kind}://${auth}${p.host}:${p.port}`;
      const tag = p.country ? `  # country=${p.country}` : "";
      return base + tag;
    });
    const text = lines.join("\n");
    clip.write(text).then(
      () => toast.ok(`Copied ${targets.length} to clipboard`),
      (e) => toast.err("Copy failed: " + String(e)),
    );
  };

  // Import from clipboard (one per line, bulkExport format).
  const bulkImportClipboard = async () => {
    try {
      const text = await clip.read();
      if (!text.trim()) { toast.err("Clipboard is empty"); return; }
      const n = await invoke<number>("proxy_bulk_import", { text, kind: "socks5" });
      reload();
      toast.ok(`Imported ${n} prox${n === 1 ? "y" : "ies"}`);
    } catch (e) { toast.err("Import failed: " + String(e)); }
  };

  const resetProxyDrag = () => {
    setActiveProxySortId(null);
    setProxySortIndicator(null);
    setProxyPageHover(null);
  };

  const handleProxyDragOver = (event: DragOverEvent) => {
    const over = event.over;
    if (!over) {
      setProxySortIndicator(null);
      setProxyPageHover(null);
      return;
    }
    if (over.data.current?.type === "page") {
      setProxySortIndicator(null);
      setProxyPageHover(over.data.current?.direction === "previous" ? "previous" : "next");
      return;
    }
    setProxyPageHover(null);
    if (over.data.current?.type !== "sortable-row" || over.data.current?.kind !== "proxy") {
      setProxySortIndicator(null);
      return;
    }
    const targetId = String(over.id);
    if (targetId === String(event.active.id)) {
      setProxySortIndicator(null);
      return;
    }
    setProxySortIndicator({
      id: targetId,
      placement: dropPlacementFor(event, pagedProxies.map((proxy) => proxy.id)),
    });
  };

  const handleProxyDragEnd = async (event: DragEndEvent) => {
    const activeId = String(event.active.id);
    const over = event.over;
    resetProxyDrag();
    if (
      !over
      || activeId === String(over.id)
      || over.data.current?.type !== "sortable-row"
      || over.data.current?.kind !== "proxy"
    ) return;

    const anchorId = String(over.id);
    const placement = dropPlacementFor(event, pagedProxies.map((proxy) => proxy.id));
    const previous = proxies;
    setProxies(moveByAnchor(proxies, activeId, anchorId, placement));
    try {
      await invoke("proxy_move_order", { id: activeId, anchorId, placement });
    } catch (error) {
      setProxies(previous);
      toast.err(`Could not save proxy order: ${String(error)}`);
    }
  };

  return (
    <DndContext
      sensors={proxySortSensors}
      collisionDetection={listCollisionDetection}
      onDragStart={(event) => setActiveProxySortId(String(event.active.id))}
      onDragOver={handleProxyDragOver}
      onDragCancel={resetProxyDrag}
      onDragEnd={handleProxyDragEnd}
    >
    <section className="page workspace-page">
      <Topbar crumbs={["PROXYLIST", "Proxies"]} search={search} onSearch={setSearch} />
      <div className="page-title">
        <h1>Proxies</h1>
        <div className="page-actions">
          {proxySel.size > 0 && (
            <div className="bulk-bar bulk-bar-floating">
              <span>{proxySel.size} selected</span>
              <button className="btn-ghost btn-sm" onClick={bulkTest}><Icon.Refresh /> Test</button>
              <button className="btn-ghost btn-sm" onClick={bulkExport}><Icon.Upload /> Export</button>
              <button
                className="btn-ghost btn-sm"
                onClick={bulkDelete}
                disabled={selectedHasLockedProxy}
                title={selectedHasLockedProxy ? "Stop browsers using selected proxies before deleting" : "Delete selected proxies"}
              ><Icon.Trash /> Delete</button>
            </div>
          )}
          <button className="btn-ghost" onClick={bulkImportClipboard} title="Import proxies from the clipboard"><Icon.Download /> Import</button>
          <button className="btn-primary" onClick={() => setBulkOpen(true)}>+ New proxy</button>
        </div>
      </div>
      <div className="rows">
        <div className="rows-head p-cols">
          <div className="sort-head" title="Drag rows to reorder"><Icon.Grip /></div>
          <div>
            <input
              type="checkbox"
              title="Select all on this page"
              // Page-only header toggle (matches profile table behaviour).
              checked={pagedProxies.length > 0 && pagedProxies.every((p) => proxySel.has(p.id))}
              ref={(el) => {
                if (!el) return;
                const any = pagedProxies.some((p) => proxySel.has(p.id));
                const all = pagedProxies.length > 0 && pagedProxies.every((p) => proxySel.has(p.id));
                el.indeterminate = any && !all;
              }}
              onChange={(e) => {
                setProxySel((prev) => {
                  const next = new Set(prev);
                  if (e.target.checked) {
                    for (const p of pagedProxies) next.add(p.id);
                  } else {
                    for (const p of pagedProxies) next.delete(p.id);
                  }
                  return next;
                });
              }}
            />
          </div>
          <div>Name</div><div>Type</div><div>Host:Port</div><div>Country</div><div>Profiles</div><div>Test result</div><div className="head-actions">ACTIONS</div>
        </div>
        <SortableContext items={pagedProxies.map((proxy) => proxy.id)} strategy={verticalListSortingStrategy}>
        {pagedProxies.map((p) => {
          const r = snapshots[p.id];
          const isBusy = !!busy[p.id];
          const cc = r?.country_code || p.country || "";
          const isSel = proxySel.has(p.id);
          const isLocked = lockedProxyIds.has(p.id);
          return (
            <SortableRow
              key={p.id}
              id={p.id}
              kind="proxy"
              className="row-wrap"
              rowClassName="row p-cols proxy-data-row"
              dropPlacement={proxySortIndicator?.id === p.id ? proxySortIndicator.placement : null}
              disabledReason={isLocked ? "Stop the browser using this proxy before reordering" : search.trim() ? "Clear search to reorder proxies" : renaming?.id === p.id ? "Finish renaming before reordering" : undefined}
              onContextMenu={(e) =>
                ctx.open(e, [
                  { label: "Test (TCP/UDP/geo)", onClick: () => fullTest(p) },
                  { label: "View details", onClick: () => setInfoFor({ proxy: p, anchor: { x: e.clientX, y: e.clientY } }) },
                  {
                    label: "Edit",
                    onClick: () => setEditing(p),
                    disabled: isLocked,
                    title: isLocked ? "Stop the browser using this proxy before editing" : "Edit proxy",
                  },
                  { sep: true, label: "", onClick: () => {} },
                  {
                    label: "Delete",
                    onClick: () => remove(p.id),
                    danger: true,
                    disabled: isLocked,
                    title: isLocked ? "Stop the browser using this proxy before deleting" : "Delete proxy",
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
                      title={isLocked ? "Stop the browser using this proxy before renaming" : "Click to rename"}
                    >
                      {p.name || "—"}
                    </span>
                  )}
                </div>
                <div><span className={`badge badge-${p.kind}`}>{p.kind}</span></div>
                <div className="cell-hostport">
                  <span
                    className={`mono small ${isLocked ? "cell-locked" : "cell-click"}`}
                    onClick={isLocked ? undefined : () => setEditing(p)}
                    title={isLocked ? "Stop the browser using this proxy before editing" : "Edit proxy"}
                  >
                    {p.host}:{p.port}
                  </span>
                </div>
                <div>
                  {cc ? (
                    <span className="proxy-country">
                      <CountryFlag cc={cc} />
                      <span className="flag">{cc}</span>
                    </span>
                  ) : <span className="muted small">—</span>}
                </div>
                <div>
                  <span className="profile-count" title={`${profileCountByProxy[p.id] ?? 0} profile(s) bound to this proxy`}>
                    {profileCountByProxy[p.id] ?? 0}
                  </span>
                </div>
                <div className="proxy-test-cell">
                  {!r && !isBusy && <span className="muted small">not tested</span>}
                  {isBusy && <span className="muted small">testing…</span>}
                  {r && !isBusy && (
                    <div className="proxy-test">
                      <span
                        className={`status-pill ${r.tcp_ms != null ? "status-active" : "status-failed"}`}
                        title={r.tcp_ms != null ? `TCP ${r.tcp_ms} ms` : "TCP failed"}
                      >
                        {r.tcp_ms != null ? "Active" : "Failed"}
                      </span>
                      {/* UDP pill: clickable to docs explaining what the
                          presence/absence of UDP means for QUIC + WebRTC.
                          Shown for any proxy type — HTTP proxies never
                          have UDP, but the badge still tells the user why
                          QUIC will be force-disabled at launch. */}
                      {r.udp_ms != null && p.kind === "socks5" && (
                        <button
                          type="button"
                          className="status-pill status-udp status-link"
                          title={`UDP relay works (${r.udp_ms} ms) — QUIC enabled at launch. Click for docs.`}
                          onClick={() => { openUrl(UDP_DOCS_URL).catch(() => {}); }}
                        >
                          UDP
                        </button>
                      )}
                      {r.udp_ms == null && (
                        <button
                          type="button"
                          className="status-pill status-no-udp status-link"
                          title="No UDP support — QUIC/HTTP-3 disabled at launch. Click for docs."
                          onClick={() => { openUrl(UDP_DOCS_URL).catch(() => {}); }}
                        >
                          UDP
                        </button>
                      )}
                      {r.tcp_ms != null && r.ip && (
                        <span className="test-ip mono small" title={r.isp}>{r.ip}</span>
                      )}
                    </div>
                  )}
                </div>
                <div className="row-actions">
                  <button
                    className="icon-btn"
                    onClick={(e) => setInfoFor({ proxy: p, anchor: { x: e.clientX, y: e.clientY } })}
                    title="Details + history"
                  ><Icon.Info /></button>
                  <button className="icon-btn" onClick={() => fullTest(p)} disabled={isBusy} title="Test TCP + UDP + geo"><Icon.Refresh /></button>
                  <button
                    className="icon-btn"
                    onClick={() => setEditing(p)}
                    disabled={isLocked}
                    title={isLocked ? "Stop the browser using this proxy before editing" : "Edit"}
                  ><Icon.Edit /></button>
                  <button
                    className="icon-btn danger"
                    onClick={() => remove(p.id)}
                    disabled={isLocked}
                    title={isLocked ? "Stop the browser using this proxy before deleting" : "Delete"}
                  ><Icon.Trash /></button>
                </div>
            </SortableRow>
          );
        })}
        </SortableContext>
        {proxies.length === 0 && (
          <div className="empty-rich">
            <div className="empty-shard"><IconWire /></div>
            <h3>No proxies yet</h3>
            <p>Add a SOCKS5/HTTP(S) endpoint so profiles can route through it.</p>
            <div className="empty-cta">
              <button className="btn-primary" onClick={() => setBulkOpen(true)}>+ New proxy</button>
            </div>
          </div>
        )}
      </div>
      {proxyPageCount > 1 && (
        <div className="pager">
          <PageDropButton
            dropId="proxy-page-previous"
            direction="previous"
            disabled={proxyPage <= 1}
            onClick={() => setProxyPage((p) => Math.max(1, p - 1))}
          >‹ Prev</PageDropButton>
          <span className="pager-info">Page {proxyPage} of {proxyPageCount} · {proxies.length} proxies</span>
          <PageDropButton
            dropId="proxy-page-next"
            direction="next"
            disabled={proxyPage >= proxyPageCount}
            onClick={() => setProxyPage((p) => Math.min(proxyPageCount, p + 1))}
          >Next ›</PageDropButton>
        </div>
      )}
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
    <DragOverlay>
      {activeProxySortId && (
        <div className="sort-overlay">
          <Icon.Grip />
          {proxies.find((proxy) => proxy.id === activeProxySortId)?.name || activeProxySortId.slice(0, 8)}
        </div>
      )}
    </DragOverlay>
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

  return (
    <div className="proxy-popover" style={{ left, top }} onClick={(e) => e.stopPropagation()}>
      <div className="popover-section">
        {latest?.ip ? (
          <>
            <div className="pop-row">
              <span className="pop-ico"><Icon.Globe /></span>
              <span className="mono">{latest.ip}</span>
            </div>
            <div className="pop-row">
              <span className="pop-ico">{latest.country_code ? <CountryFlag cc={latest.country_code} height={14} /> : <Icon.Globe />}</span>
              <span>{[latest.region, latest.city].filter(Boolean).join(", ") || latest.country || "—"}</span>
            </div>
            {latest.timezone && (
              <div className="pop-row">
                <span className="pop-ico"><Icon.Clock /></span>
                <span>{latest.timezone}</span>
              </div>
            )}
            {latest.isp && (
              <div className="pop-row">
                <span className="pop-ico"><Icon.Building /></span>
                <span className="muted small">{latest.isp}</span>
              </div>
            )}
            <div className="pop-row pop-row-split">
              <span className={`pop-pill ${latest.tcp_ms != null ? "ok" : "err"}`}>
                TCP {latest.tcp_ms != null ? `${latest.tcp_ms} ms` : "✗"}
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
          <div className="muted small">Not tested yet — click ↻ on the row.</div>
        )}
      </div>
      <div className="popover-divider">IP HISTORY</div>
      <div className="popover-history">
        {history.length === 0 && <div className="muted small" style={{ padding: "10px 0" }}>No history yet</div>}
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

/// Flat 4:3 country flag (flag-icons sprite); empty input renders nothing.
function CountryFlag({ cc, height = 15 }: { cc: string; height?: number }) {
  if (!cc || cc.length !== 2 || !/^[a-zA-Z]{2}$/.test(cc)) return null;
  const code = cc.toLowerCase();
  // `fi fi-XX`; omit `fis` to keep 4:3 rectangle.
  return (
    <span
      className={`fi fi-${code} flag-rect`}
      style={{ height, width: Math.round(height * 4 / 3) }}
      aria-hidden
    />
  );
}

/// "@1700000000" → "May 26, 14:30" (UTC for cross-timezone consistency).
function fmtTs(stamp: string): string {
  if (!stamp.startsWith("@")) return stamp;
  const n = parseInt(stamp.slice(1), 10);
  if (!Number.isFinite(n)) return stamp;
  const d = new Date(n * 1000);
  return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

/// Format ms uptime as "1h 23m" / "12m 30s" / "45s".
function fmtUptime(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${sec.toString().padStart(2, "0")}s`;
  return `${sec}s`;
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
    if (!text.trim()) { toast.err("Nothing to parse"); return; }
    try {
      const preview = await invoke<ProxyBulkParsePreview>("proxy_bulk_parse", { text, kind });
      setParseIssues(preview.invalid);
      setSkipped({
        duplicateLines: preview.duplicate_lines,
        existingDuplicates: preview.existing_duplicates,
      });
      if (preview.invalid.length > 0) {
        toast.err(`Fix ${preview.invalid.length} invalid proxy line${preview.invalid.length === 1 ? "" : "s"}`);
        return;
      }
      const duplicateCount = preview.duplicate_lines + preview.existing_duplicates;
      if (preview.entries.length === 0) {
        toast.info(duplicateCount > 0 ? "No new proxies: every valid line is a duplicate" : "No valid proxy lines found");
        return;
      }
      setRows(preview.entries.map((entry) => ({ entry, selected: true, status: "idle" })));
      if (duplicateCount > 0) {
        toast.info(`Skipped ${duplicateCount} duplicate proxy line${duplicateCount === 1 ? "" : "s"}`);
      }
    } catch (e) { toast.err(String(e)); }
  };

  const testOne = async (idx: number) => {
    setRows((rs) => rs.map((r, i) => i === idx ? { ...r, status: "testing" } : r));
    const entry = rows[idx]?.entry;
    if (!entry) return;
    try {
      const snap = await invoke<ProxyTestSnapshot>("proxy_full_test", { entry });
      setRows((rs) => rs.map((r, i) =>
        i === idx
          ? {
              ...r,
              status: snap.tcp_ms != null ? "ok" : "fail",
              tcp_ms: snap.tcp_ms,
              udp_ms: snap.udp_ms,
              country: snap.country_code || r.country,
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
      const results = await invoke<ProxyBatchTestResult[]>("proxy_full_test_batch", {
        entries: rows.map((row) => row.entry),
      });
      const byIndex = new Map(results.map((result) => [result.index, result]));
      setRows((current) => current.map((row, index) => {
        const result = byIndex.get(index);
        const snap = result?.snapshot;
        if (!snap) {
          return {
            ...row,
            status: "incomplete",
            error: result?.error ?? "Proxy test failed",
          };
        }
        return {
          ...row,
          status: snap.tcp_ms != null ? "ok" : "fail",
          tcp_ms: snap.tcp_ms,
          udp_ms: snap.udp_ms,
          country: snap.country_code || row.country,
          error: snap.tcp_ms == null
            ? (result?.error ?? "Proxy latency test failed")
            : undefined,
          entry: { ...row.entry, country: snap.country_code || row.entry.country },
        };
      }));
    } catch (e) {
      setRows((current) => current.map((row) => ({
        ...row,
        status: "incomplete",
        error: String(e),
      })));
    } finally {
      setBusy(false);
    }
  };

  const saveSelected = async () => {
    const entries = rows.filter((r) => r.selected).map((r) => r.entry);
    if (entries.length === 0) { toast.err("Nothing selected"); return; }
    try {
      const n = await invoke<number>("proxy_bulk_save", { entries });
      const skippedOnSave = entries.length - n;
      if (skippedOnSave > 0) {
        toast.info(`Imported ${n}; skipped ${skippedOnSave} duplicate${skippedOnSave === 1 ? "" : "s"}`);
      } else {
        toast.ok(`Imported ${n} prox${n === 1 ? "y" : "ies"}`);
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
          <h2><ShardMini /> Bulk import proxies</h2>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </header>
        <div className="dialog-body">
          {rows.length === 0 ? (
            <>
              <label>
                <span className="lbl">Default type (used when a line has no scheme)</span>
                <select value={kind} onChange={(e) => setKind(e.target.value as ProxyEntry["kind"])}>
                  <option value="socks5">SOCKS5</option>
                  <option value="http">HTTP</option>
                  <option value="https">HTTPS</option>
                </select>
              </label>
              <label>
                <span className="lbl">Paste one proxy per line</span>
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
              <p className="muted small">
                Invalid lines must be fixed. Duplicates use type, host, port, and username and are skipped automatically.
              </p>
              {parseIssues.length > 0 && (
                <div className="bulk-parse-errors" role="alert">
                  <strong>Invalid proxy format</strong>
                  <ul>
                    {parseIssues.slice(0, 6).map((issue) => (
                      <li key={`${issue.line}:${issue.reason}`}>Line {issue.line}: {issue.reason}</li>
                    ))}
                    {parseIssues.length > 6 && <li>And {parseIssues.length - 6} more invalid lines</li>}
                  </ul>
                </div>
              )}
              {skippedCount > 0 && rows.length === 0 && (
                <div className="bulk-dedupe-note">
                  Found {skippedCount} duplicate{skippedCount === 1 ? "" : "s"}: {skipped.duplicateLines} repeated in this input, {skipped.existingDuplicates} already saved. They will not be imported.
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
                  <span>{selCount} of {rows.length} selected</span>
                </label>
                <div style={{ marginLeft: "auto", display: "flex", gap: 6 }}>
                  <button
                    className="btn-ghost btn-sm"
                    onClick={() => {
                      setRows([]);
                      setSkipped({ duplicateLines: 0, existingDuplicates: 0 });
                    }}
                  >← Back</button>
                  <button className="btn-ghost btn-sm" onClick={testAll} disabled={busy}>
                    {busy ? "Testing…" : <><Icon.Refresh /> Test all</>}
                  </button>
                  <button
                    className="btn-ghost btn-sm"
                    onClick={() =>
                      setRows((rs) =>
                        rs.map((r) => ({ ...r, selected: r.status === "ok" }))
                      )
                    }
                    title="Tick only proxies whose latest test succeeded"
                  >
                    ✓ Keep working only
                  </button>
                </div>
              </div>
              {skippedCount > 0 && (
                <div className="bulk-dedupe-note">
                  Skipped {skippedCount} duplicate{skippedCount === 1 ? "" : "s"}
                  {skipped.existingDuplicates > 0 ? ` (${skipped.existingDuplicates} already saved)` : ""}.
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
                      {r.status === "idle" && <span className="muted small">not tested</span>}
                      {r.status === "testing" && <span className="muted small">testing…</span>}
                      {r.status === "ok" && (
                        <>
                          <span className="status-pill status-active" title={`TCP ${r.tcp_ms} ms`}>Active</span>
                          {r.entry.kind === "socks5" && r.udp_ms != null && (
                            <span className="status-pill status-udp" title={`UDP relay works (${r.udp_ms} ms)`}>UDP</span>
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
                        <span className="status-pill status-failed" title={r.error}>Failed</span>
                      )}
                      {r.status === "incomplete" && (
                        <span className="status-pill" title={r.error}>Incomplete</span>
                      )}
                    </div>
                    <button className="btn-sm btn-ghost icon-only" onClick={() => testOne(i)} disabled={r.status === "testing"} title="Test this row"><Icon.Refresh /></button>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>Cancel</button>
          {rows.length === 0 ? (
            <button className="btn-primary" onClick={parse}><ShardMini /> Parse →</button>
          ) : (
            <button className="btn-primary" onClick={saveSelected}>
              <ShardMini /> Import {selCount}
            </button>
          )}
        </footer>
      </div>
    </DialogBackdrop>
  );
}

function ProxyEditor({ initial, onClose }: { initial: ProxyEntry; onClose: () => void }) {
  const [p, setP] = useState<ProxyEntry>(initial);
  const save = async () => {
    try {
      await invoke("proxy_save", { entry: p });
      toast.ok(initial.id ? "Proxy saved" : "Proxy added");
      onClose();
    } catch (e) { toast.err(String(e)); }
  };
  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog">
        <header className="dialog-head">
          <h2><ShardMini /> {initial.id ? "Edit proxy" : "New proxy"}</h2>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </header>
        <div className="dialog-body">
          <Field label="Name" value={p.name} onChange={(v: string) => setP({ ...p, name: v })} />
          <div className="form-row">
            <label>
              <span className="lbl">Type</span>
              <select value={p.kind} onChange={(e) => setP({ ...p, kind: e.target.value as ProxyEntry["kind"] })}>
                <option value="socks5">SOCKS5</option><option value="http">HTTP</option><option value="https">HTTPS</option>
              </select>
            </label>
            <Field label="Country" value={p.country} onChange={(v: string) => setP({ ...p, country: v })} />
          </div>
          <div className="form-row">
            <Field label="Host" value={p.host} onChange={(v: string) => setP({ ...p, host: v })} />
            <NumField label="Port" value={p.port} onChange={(v) => setP({ ...p, port: v as any })} />
          </div>
          <div className="form-row">
            <Field label="Username" value={p.username} onChange={(v: string) => setP({ ...p, username: v })} />
            <Field label="Password" value={p.password} onChange={(v: string) => setP({ ...p, password: v })} type="password" />
          </div>
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>Cancel</button>
          <button className="btn-primary" onClick={save}><ShardMini /> Save</button>
        </footer>
      </div>
    </DialogBackdrop>
  );
}

function FingerprintsView() {
  const [items, setItems] = useState<FingerprintEntry[]>([]);
  const [importerOpen, setImporterOpen] = useState(false);

  const reload = () =>
    invoke<FingerprintEntry[]>("fingerprint_list").then(setItems).catch((e) => toast.err(String(e)));
  useEffect(() => { reload(); }, []);

  const use = async (id: string) => {
    try {
      const meta = await invoke<ProfileMeta>("profile_create_from_template", { templateId: id });
      toast.ok(`Created "${meta.name}" — open Browsers to edit`);
    } catch (e) {
      toast.err(String(e));
    }
  };

  const remove = async (id: string) => {
    if ((await confirmModal({ title: "Remove fingerprint", message: "Remove this fingerprint from the library?", danger: true })) !== true) return;
    try {
      await invoke("fingerprint_delete", { id });
      toast.ok("Removed");
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  const importJsonFile = async () => {
    try {
      const txt = await pickJsonText();
      if (txt === null) return;
      const e = await invoke<FingerprintEntry>("fingerprint_import", { jsonText: txt, idHint: null });
      toast.ok(`Imported "${e.label}"`);
      reload();
    } catch (e) { toast.err(String(e)); }
  };

  return (
    <section className="page workspace-page">
      <Topbar crumbs={["Library", "Fingerprints"]} search="" onSearch={() => {}} />
      <div className="page-title">
        <h1>Fingerprint Library</h1>
        <div className="page-actions">
          <button
            className="btn-ghost"
            onClick={async () => {
              try {
                await invoke("open_fingerprint_dir");
              } catch (e) { toast.err(String(e)); }
            }}
            title="Reveal the on-disk library folder; drop JSONs here to add them"
          >
            <Icon.Folder /> Library folder
          </button>
          <button className="btn-ghost" onClick={importJsonFile}><Icon.Folder /> Import from file</button>
          <button className="btn-primary" onClick={() => setImporterOpen(true)}>+ Paste JSON</button>
        </div>
      </div>
      <p className="muted small" style={{ marginBottom: 14 }}>
        These FingerprintConfig snapshots populate the <strong>GPU</strong> select in the profile editor.
        Import your own from any working ShardX profile JSON to expand the list.
      </p>
      {items.length === 0 ? (
        <div className="empty">Library is empty — click "Import from file" or "Paste JSON".</div>
      ) : (
        <LibraryGroups items={items} onUse={use} onRemove={remove} />
      )}
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
            <h3>{platform}</h3>
            <span className="lib-group-count">{list.length}</span>
          </div>
          <div className="lib-grid">
            {list.map((t) => (
              <div
                key={t.id}
                className="lib-card"
                style={{ ['--accent' as any]: t.tag_color }}
              >
                <div className="lib-card-head">
                  <span className="lib-label">{t.label}</span>
                  {t.chrome && <span className="lib-chrome">Chrome {t.chrome}</span>}
                </div>
                <div className="lib-gpu mono" title={t.gpu}>{t.gpu || "—"}</div>
                <div className="lib-card-foot">
                  <button className="btn-sm btn-ghost" onClick={() => onUse(t.id)}>Use →</button>
                  {t.builtin
                    ? <span className="lib-tag">built-in</span>
                    : <button className="btn-sm btn-ghost danger" onClick={() => onRemove(t.id)} title="Remove">✕</button>}
                </div>
              </div>
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
      toast.ok(`Imported "${e.label}"`);
      onClose();
    } catch (e) { toast.err(String(e)); }
  };
  return (
    <DialogBackdrop onClose={onClose} dismissOnBackdrop={false}>
      <div className="dialog dialog-wide">
        <header className="dialog-head">
          <h2><ShardMini /> Paste FingerprintConfig JSON</h2>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </header>
        <div className="dialog-body">
          <Field label="Name (optional, becomes the file id)" value={name} onChange={setName} placeholder="e.g. mac-m4-pro-real" />
          <label>
            <span className="lbl">Paste the full JSON</span>
            <textarea rows={14} className="mono" value={text} onChange={(e) => setText(e.target.value)} placeholder='{ "name": "...", "navigator": { ... }, "webgl": { ... }, ... }' />
          </label>
        </div>
        <footer className="dialog-foot">
          <button className="btn-ghost" onClick={onClose}>Cancel</button>
          <button className="btn-primary" onClick={save}><ShardMini /> Import</button>
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
          <h2><ShardMini /> {mode === "move" ? "Move to folder" : "New folder"}</h2>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </header>
        <div className="dialog-body">
          {showList && (
            <>
              <span className="lbl">Existing folders</span>
              <div className="folder-pick-list">
                {existing.map((f) => (
                  <button key={f} className="folder-pick" onClick={() => onPick(f)}>
                    <Icon.Folder /> {f}
                  </button>
                ))}
              </div>
              <div className="folder-pick-sep"><span>or create new</span></div>
            </>
          )}
          <label>
            <span className="lbl">{showList ? "New folder name" : "Folder name"}</span>
            <input
              ref={ref}
              value={name}
              placeholder="e.g. Shops, Socials, QA…"
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") create();
                if (e.key === "Escape") onClose();
              }}
            />
          </label>
          {dup && <div className="muted small" style={{ color: "var(--err)" }}>Folder “{trimmed}” already exists.</div>}
          <div style={{ display: "flex", justifyContent: "flex-end", gap: 10, marginTop: 4 }}>
            <button className="btn-ghost" onClick={onClose}>Cancel</button>
            <button className="btn-primary" disabled={!trimmed || dup} onClick={create}>
              {showList ? "Create & move" : "Create"}
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
  const [repairRequested, setRepairRequested] = useState(false);
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

      // Subscribe immediately before install so the local startup check never
      // waits on event registration, while still preserving the first event.
      const stopProgress = await listen<RtProgress>("runtime:progress", (e) => {
        if (!cancelled) setProg(e.payload);
      });
      if (cancelled) {
        stopProgress();
        return;
      }
      unProg = stopProgress;

      try {
        const result = await invoke<RtStatus>("runtime_install", { force });
        if (cancelled) return;
        if (!result.installed || !result.fingerprints_installed || !result.widevine_installed) {
          throw new Error("Browser runtime installation did not complete");
        }
        setProg(null);
        setPhase("ready");
      } catch (e: any) {
        if (!cancelled) setErr(typeof e === "string" ? e : (e?.message ?? String(e)));
      }
    };

    (async () => {
      // Remove the pending marker used by older builds. Runtime updates are now
      // checked only from Settings and are never installed automatically.
      localStorage.removeItem("shardx-runtime-update-pending");
      let status: RtStatus;
      try {
        status = await invoke<RtStatus>("runtime_local_status");
      } catch (e: any) {
        if (!cancelled) setErr(String(e));
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
        && status.widevine_installed;
      if (complete) {
        setPhase("ready");
        return;
      }

      // Only a genuinely new installation starts downloading automatically.
      // A previously initialized but damaged runtime waits for explicit repair.
      if (!status.initialized && !status.installed) {
        await installRuntime("setup", false);
      } else if (repairRequested) {
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
  }, [repairRequested]);

  if (phase === "ready") {
    return <>{children}</>;
  }

  if (phase === "checking" && !showChecking && !err) return null;

  const copy = phase === "checking"
    ? {
        title: "Starting ShardX",
        description: "Checking the local browser runtime…",
      }
    : phase === "repair-required"
      ? {
          title: "Browser runtime needs repair",
          description: "Required local files are missing or incomplete. Nothing will be downloaded until you choose Repair.",
        }
      : installMode === "repair"
        ? {
            title: "Repairing ShardX browser",
            description: "Restoring missing browser runtime files.",
          }
        : {
            title: "Setting up ShardX browser",
            description: "Downloading the browser runtime for this installation.",
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
              <span>{prog.label}</span>
              <span>
                {prog.phase === "download"
                  ? prog.total > 0
                    ? `${fmt(prog.received)} / ${fmt(prog.total)} (${prog.percent}%)`
                    : "Starting download…"
                  : "Extracting…"}
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
            Preparing browser runtime…
          </div>
        )}
        {err && (
          <div className="runtime-gate-error">{err}</div>
        )}
        {phase === "repair-required" && !err && (
          <button className="btn-primary runtime-repair-btn" onClick={() => setRepairRequested(true)}>
            <Icon.Refresh /> Repair browser runtime
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
  fingerprints_installed: boolean;
  fingerprints_update_available: boolean;
  widevine_installed: boolean;
  widevine_update_available: boolean;
};

type RuntimeUpdateTone = "unchecked" | "current" | "available" | "missing";

function RuntimeUpdateRow({
  label,
  detail,
  tone,
}: {
  label: string;
  detail: string;
  tone: RuntimeUpdateTone;
}) {
  const status = tone === "current"
    ? "Up to date"
    : tone === "available"
      ? "Update available"
      : tone === "missing"
        ? "Missing files"
        : "Not checked";
  return (
    <div className="runtime-update-row">
      <div className="runtime-update-copy">
        <span className="runtime-update-label">{label}</span>
        <span className="runtime-update-detail">{detail}</span>
      </div>
      <span className={`runtime-update-state runtime-update-${tone}`}>{status}</span>
    </div>
  );
}

function RuntimeUpdateCard() {
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<RuntimeUpdateCheck | null>(null);
  const [error, setError] = useState<string | null>(null);

  const check = async () => {
    setChecking(true);
    setResult(null);
    setError(null);
    try {
      setResult(await invoke<RuntimeUpdateCheck>("runtime_check_updates"));
    } catch (e: any) {
      setError(typeof e === "string" ? e : (e?.message ?? String(e)));
    } finally {
      setChecking(false);
    }
  };

  const tone = (installed: boolean, available: boolean): RuntimeUpdateTone => {
    if (!result) return "unchecked";
    if (!installed) return "missing";
    return available ? "available" : "current";
  };

  const chromiumDetail = result
    ? result.chromium_installed
      ? `Installed ${result.chromium_installed_version ?? "unknown"} · Latest ${result.chromium_latest_version ?? "unknown"}`
      : "The local Chromium runtime is incomplete."
    : "Compare the installed browser engine with the latest runtime manifest.";

  return (
    <div className="card settings-card runtime-update-card">
      <div className="runtime-update-head">
        <div className="settings-card-heading">
          <h3>Runtime update check</h3>
          <p className="muted small">
            Checks run only when you press the button. ShardX will not download or install updates automatically.
          </p>
        </div>
        <button className="btn-ghost runtime-update-check" onClick={check} disabled={checking}>
          <Icon.Refresh /> {checking ? "Checking…" : "Check for updates"}
        </button>
      </div>

      <div className="runtime-update-list">
        <RuntimeUpdateRow
          label="Chromium browser runtime"
          detail={chromiumDetail}
          tone={tone(result?.chromium_installed ?? true, result?.chromium_update_available ?? false)}
        />
        <RuntimeUpdateRow
          label="Fingerprint library"
          detail={result?.fingerprints_installed === false
            ? "The bundled fingerprint templates are incomplete."
            : "Bundled multi-platform fingerprint templates."}
          tone={tone(result?.fingerprints_installed ?? true, result?.fingerprints_update_available ?? false)}
        />
        <RuntimeUpdateRow
          label="Widevine CDM"
          detail={result?.widevine_installed === false
            ? "Required Widevine runtime files are incomplete."
            : "DRM component bundled with the browser runtime."}
          tone={tone(result?.widevine_installed ?? true, result?.widevine_update_available ?? false)}
        />
      </div>

      {error && <div className="runtime-update-error">{error}</div>}
      <div className="runtime-update-note">
        ShardX Launcher itself is excluded from update checks. This card never downloads or installs files.
      </div>
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
      title={version ? `Running ${version}` : "Version unavailable"}
    >
      <ShardMini />
      <div className="version-pill-text">
        <div className="version-pill-current">
          ShardX Launcher v{version ?? "…"}
        </div>
        <div className="version-pill-sub">update checks disabled</div>
      </div>
    </button>
  );
}

function SettingsView() {
  const [s, setS] = useState<Settings>({
    theme: "dark",
    geo_checker: "ip-api.com",
    screen_resolution_mode: "fingerprint",
    api_enabled: true,
    api_port: 40325,
  });
  const [api, setApi] = useState<ApiInfo | null>(null);
  const refreshApi = () => invoke<ApiInfo>("api_info").then(setApi).catch(() => {});
  useEffect(() => { invoke<Settings>("settings_get").then(setS); refreshApi(); }, []);
  const regenToken = async () => {
    try { setApi(await invoke<ApiInfo>("api_regenerate_token")); toast.ok("Token regenerated"); }
    catch (e) { toast.err(String(e)); }
  };

  const [mcpBusy, setMcpBusy] = useState(false);
  // Download MCP server source into the portable app data directory.
  const downloadMcp = async () => {
    setMcpBusy(true);
    try {
      const path = await invoke<string>("mcp_download");
      toast.ok(`MCP downloaded to ${path}`);
    } catch (e) { toast.err("MCP download failed: " + String(e)); }
    finally { setMcpBusy(false); }
  };
  const save = async () => {
    try { await invoke("settings_save", { value: s }); toast.ok("Settings saved"); }
    catch (e) { toast.err(String(e)); }
  };
  return (
    <section className="page settings-page">
      <Topbar crumbs={["System", "Settings"]} search="" onSearch={() => {}} />
      <div className="page-title"><h1>Settings</h1></div>

      <div className="settings-card-list">
        <div className="card settings-card">
          <h3>Proxy geo checker</h3>
          <p className="muted small">Which free public IP-geo service to hit when you press the proxy <strong>Test</strong> button. All three are no-key, rate-limited.</p>
          <label>
            <span className="lbl">Provider</span>
            <select value={s.geo_checker ?? "ip-api.com"} onChange={(e) => setS({ ...s, geo_checker: e.target.value })}>
              <option value="ip-api.com">ip-api.com (45 req/min, HTTP)</option>
              <option value="ipapi.co">ipapi.co (1k/day, HTTPS)</option>
              <option value="ipwho.is">ipwho.is (10k/month, HTTPS)</option>
            </select>
          </label>
        </div>

      <div className="card settings-card">
        <h3>Screen resolution</h3>
        <p className="muted small">
          <strong>From fingerprint</strong> reports the screen carried in the bound profile (recommended for anti-detect coherence).
          <strong> Real</strong> lets ShardX expose the host monitor's actual size.
        </p>
        <label>
          <span className="lbl">Mode</span>
          <select
            value={s.screen_resolution_mode ?? "fingerprint"}
            onChange={(e) => setS({ ...s, screen_resolution_mode: e.target.value })}
          >
            <option value="fingerprint">From fingerprint</option>
            <option value="real">Real (host monitor)</option>
          </select>
        </label>
      </div>

      <div className="card settings-card">
        <h3>Automation API</h3>
        <p className="muted small">
          Local HTTP API (axum) for scripting — create/launch/close profiles
          and get a CDP WebSocket URL. Binds <strong>127.0.0.1</strong> only,
          JWT Bearer auth. Changes to enable/port apply after restarting the app.{" "}
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              openUrl(withUtm("https://docs.proxyshard.com/eng/shardx-launcher-api/binding-and-lifecycle?fallback=true")).catch(() => {});
            }}
          >
            Full API reference →
          </a>
        </p>
        <label className="row-inline">
          <input
            type="checkbox"
            checked={s.api_enabled ?? true}
            onChange={(e) => setS({ ...s, api_enabled: e.target.checked })}
          />
          <span className="lbl">Enable API server</span>
        </label>
        <label>
          <span className="lbl">Port</span>
          <input
            type="number"
            value={s.api_port ?? 40325}
            onChange={(e) => setS({ ...s, api_port: Number(e.target.value) || 40325 })}
          />
        </label>
        {api && (
          <>
            <label>
              <span className="lbl">Base URL</span>
              <CopyField value={api.base_url} />
            </label>
            <label>
              <span className="lbl">Bearer token</span>
              <CopyField value={api.token} secret />
            </label>
            <div className="row-inline settings-card-inline-action">
              <button className="btn-ghost" onClick={regenToken}>Regenerate token</button>
              <span className="muted small">Invalidates the current token immediately.</span>
            </div>
            <p className="muted small settings-card-footnote">
              Send it as <code>Authorization: Bearer &lt;token&gt;</code>.
            </p>
          </>
        )}
      </div>

      <div className="card settings-card">
        <h3>MCP server</h3>
        <p className="muted small">
          Download the <strong>MCP</strong> server source (lets an AI client drive
          profiles and a CDP browser) into a folder you choose. The app does not run
          it — install its deps and register it with your MCP client per the included
          README. Requires Node.js.
        </p>
        <button className="btn-ghost" onClick={downloadMcp} disabled={mcpBusy}>
          <Icon.Download /> {mcpBusy ? "Downloading…" : "Download MCP server"}
        </button>
      </div>

        <RuntimeUpdateCard />
      </div>

      <div className="card-actions">
        <button className="btn-primary" onClick={async () => { await save(); refreshApi(); }}><ShardMini /> Save settings</button>
      </div>
    </section>
  );
}
