export type AssignPair = { profileId: string; proxyId: string };

/** Stable list-order pairing. Unassigned profiles retain their current proxy. */
export function planAssignment(
  profileIds: readonly string[],
  proxyIds: readonly string[],
  allowSharing: boolean,
  usedBy: Readonly<Record<string, readonly string[]>>,
) {
  const targets = new Set(profileIds);
  const skippedShared = proxyIds.filter((id) => !allowSharing && (usedBy[id] ?? []).some((p) => !targets.has(p)));
  const available = proxyIds.filter((id) => !skippedShared.includes(id));
  // Profiles left over retain their proxy, so their binding also reserves it.
  let count = allowSharing && available.length ? profileIds.length : Math.min(profileIds.length, available.length);
  let usable = available;
  if (!allowSharing) {
    while (true) {
      const changing = new Set(profileIds.slice(0, count));
      usable = available.filter((id) => (usedBy[id] ?? []).every((p) => changing.has(p)));
      const next = Math.min(count, usable.length);
      if (next === count) break;
      count = next;
    }
  }
  const pairs: AssignPair[] = profileIds.slice(0, count).map((profileId, index) => ({
    profileId, proxyId: usable[index % usable.length],
  }));
  return { pairs, left: profileIds.slice(count), skippedShared: available.filter((id) => !usable.includes(id)).concat(skippedShared) };
}
