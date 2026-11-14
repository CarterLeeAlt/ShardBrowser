import { useSyncExternalStore } from "react";
import zh from "./zh.json";

export type Language = "en" | "zh";
const STORAGE_KEY = "shardx-language";
const saved = localStorage.getItem(STORAGE_KEY);
let language: Language = saved === "en" || saved === "zh" ? saved : navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
const subscribers = new Set<() => void>();
const subscribe = (callback: () => void) => { subscribers.add(callback); return () => { subscribers.delete(callback); }; };
const snapshot = () => language;
document.documentElement.lang = language === "zh" ? "zh-CN" : "en";

export function setLanguage(value: Language) {
  localStorage.setItem(STORAGE_KEY, value);
  language = value;
  document.documentElement.lang = value === "zh" ? "zh-CN" : "en";
  subscribers.forEach((callback) => callback());
}

export function useLanguage() { return useSyncExternalStore(subscribe, snapshot); }

/** English source strings are the keys; arguments are interpolated as text. */
export function tr(message: string, ...args: unknown[]): string {
  const text = language === "zh" ? (zh as Record<string, string>)[message] ?? message : message;
  return text.replace(/\{(\d+)\}/g, (match, index: string) => Number(index) < args.length ? String(args[Number(index)]) : match);
}
