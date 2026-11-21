import zh from "./zh.json";

/** Unknown diagnostics are shown verbatim. */
export function tr(message: string, ...args: unknown[]): string {
  const text = (zh as Record<string, string>)[message] ?? message;
  return text.replace(/\{(\d+)\}/g, (match, index: string) => Number(index) < args.length ? String(args[Number(index)]) : match);
}
