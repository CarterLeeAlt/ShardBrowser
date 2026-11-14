// Three-way merge: preserve external edits, reject overlapping field changes.
export function mergeProfileEdits(baseline: any, edited: any, current: any): any {
  const conflicts: string[] = [];
  const same = (a: any, b: any) => JSON.stringify(a) === JSON.stringify(b);
  const object = (v: any) => v !== null && typeof v === "object" && !Array.isArray(v);
  const merge = (before: any, after: any, latest: any, path: string): any => {
    if (same(before, after)) return latest;
    if (same(before, latest) || same(after, latest)) return after;
    if (object(before) && object(after) && object(latest)) {
      return Object.fromEntries([...new Set([...Object.keys(before), ...Object.keys(after), ...Object.keys(latest)])]
        .map(key => [key, merge(before[key], after[key], latest[key], path ? `${path}.${key}` : key)])
        .filter(([, value]) => value !== undefined));
    }
    conflicts.push(path);
    return latest;
  };
  const result = merge(baseline, edited, current, "");
  if (conflicts.length) throw new Error(conflicts.join(", "));
  return result;
}
