// Resolve the version a release run should publish.
//
// Used by .github/workflows/release.yml. Modes:
//   * explicit  — `--explicit v2.0.5` publishes exactly that tag (manual runs)
//   * auto      — publishes `--base` (the repo's version files), bumping past
//                 any tag that already exists
// After a successful publish the workflow writes bump(base) back into the
// version files, so the next push publishes the next number: 2.0.2 → 2.0.3.
//
// Increment rules: patch and minor roll over at 9 (2.0.9 → 2.1.0,
// 2.9.9 → 3.0.0); major is unbounded.
//
// Usage:
//   node scripts/next-release-version.mjs --base 2.0.2 --taken "v1.0.1 v1.0.2"
//   node scripts/next-release-version.mjs --base 2.0.2 --explicit v3.1.4
//   node scripts/next-release-version.mjs --base 2.0.9 --next   (post-release write-back)
// Output (KEY=VALUE lines, consumed via GITHUB_OUTPUT):
//   tag=v2.0.3
//   version=2.0.3

import { parseArgs } from "node:util";

const TAG_RE = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const VER_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

function bump(version) {
  let [major, minor, patch] = version.split(".").map(Number);
  patch += 1;
  if (patch > 9) {
    patch = 0;
    minor += 1;
  }
  if (minor > 9) {
    minor = 0;
    major += 1;
  }
  return `${major}.${minor}.${patch}`;
}

const { values } = parseArgs({
  options: {
    base: { type: "string" },
    explicit: { type: "string" },
    taken: { type: "string", default: "" },
    // Resolve the version AFTER the input one instead of publishing the input
    // (used to write the next number back into the version files).
    next: { type: "boolean", default: false },
  },
});

const taken = new Set(
  values.taken
    .split(/\s+/)
    .filter(Boolean),
);

let version;
if (values.explicit) {
  if (!TAG_RE.test(values.explicit)) {
    console.error(
      `Invalid --explicit '${values.explicit}'. Expected vMAJOR.MINOR.PATCH (for example, v2.0.5).`,
    );
    process.exit(1);
  }
  version = values.explicit.slice(1);
} else {
  if (!values.base || !VER_RE.test(values.base)) {
    console.error(
      `Invalid or missing --base '${values.base ?? ""}'. Expected MAJOR.MINOR.PATCH.`,
    );
    process.exit(1);
  }
  version = values.base;
}

if (values.next) {
  version = bump(version);
} else {
  // Never republish an existing tag: keep rolling until the tag is free.
  while (taken.has(`v${version}`)) {
    version = bump(version);
  }
}

console.log(`tag=v${version}`);
console.log(`version=${version}`);
