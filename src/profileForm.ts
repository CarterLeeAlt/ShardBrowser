export type NoiseMode = "real" | "auto";
export type WebRtcMode = "auto" | "tcp_only" | "block";
export type GeoMode = "auto" | "manual";

export type ProfileForm = {
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
export const AUTO_TZ = "auto";
export const AUTO_LANG = "auto";

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

export const defaultForm = (): ProfileForm => ({
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

export function fromStored(stored: any): ProfileForm {
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
  f.do_not_track = stored?.navigator?.do_not_track === true;

  const noise = stored?.noise ?? {};
  const noiseMode = (n: any): NoiseMode => (n?.enabled ? "auto" : "real");
  f.noise_canvas = noiseMode(noise.canvas);
  f.noise_webgl = noiseMode(noise.webgl);
  f.noise_audio = noiseMode(noise.audio);
  f.noise_client_rects = noiseMode(noise.client_rects);
  f.noise_sensors = noiseMode(noise.sensors);
  // Fonts default OFF (real); mirrors C++ default.
  f.noise_fonts = noiseMode(noise.fonts);
  f.blocked_ports = Array.isArray(stored?.network?.blocked_ports)
    ? stored.network.blocked_ports.filter((n: any) => typeof n === "number")
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
export function toStored(f: ProfileForm, lib: { payload: any } | null, existing: any | null = null, baseline: ProfileForm | null = null): any {
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
  if (changed("do_not_track")) base.navigator.do_not_track = f.do_not_track;
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
  if (changed("blocked_ports")) base.network = { ...base.network, blocked_ports: [...f.blocked_ports].sort((a, b) => a - b) };

  return base;
}
