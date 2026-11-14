// Resolve the version a release run should publish.
//
// Used by .github/workflows/release.yml. Modes:
//   * explicit  — `--explicit v2.0.5` publishes exactly that tag (manual runs)
//   * auto      — `--base 2.0.5` publishes exactly the source-tree version
// Publishing preserves this version; version changes are explicit source edits.
//
// Usage:
//   node scripts/next-release-version.mjs --base 2.0.2
//   node scripts/next-release-version.mjs --explicit v3.1.4
// Output (KEY=VALUE lines, consumed via GITHUB_OUTPUT):
//   tag=v2.0.2
//   version=2.0.2

import { parseArgs } from "node:util";

const TAG_RE = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const VER_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

const { values } = parseArgs({
  options: {
    base: { type: "string" },
    explicit: { type: "string" },
  },
});

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

console.log(`tag=v${version}`);
console.log(`version=${version}`);
