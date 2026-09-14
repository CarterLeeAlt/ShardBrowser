import { parseArgs } from "node:util";
import { readFileSync, writeFileSync } from "node:fs";

const VERSION_PATTERN = "(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)";
const VERSION_RE = new RegExp(`^${VERSION_PATTERN}$`);

function fail(message) {
  console.error(`Release version bump failed: ${message}`);
  process.exit(1);
}

function replaceOne(path, pattern, replacement) {
  const content = readFileSync(path, "utf8");
  const matches = [...content.matchAll(pattern)];
  if (matches.length !== 1) {
    fail(`${path} must contain exactly one matching version declaration; found ${matches.length}`);
  }
  writeFileSync(path, content.replace(pattern, replacement), "utf8");
}

const { values, positionals } = parseArgs({
  options: {
    from: { type: "string" },
    to: { type: "string" },
  },
  strict: true,
  allowPositionals: true,
});

if (positionals.length > 0) {
  fail(`unexpected positional arguments: ${positionals.join(" ")}`);
}
if (!values.from || !VERSION_RE.test(values.from)) {
  fail(`invalid or missing --from ${JSON.stringify(values.from ?? "")}`);
}
if (!values.to || !VERSION_RE.test(values.to)) {
  fail(`invalid or missing --to ${JSON.stringify(values.to ?? "")}`);
}
if (values.from === values.to) {
  fail("--from and --to must differ");
}

const oldVersion = values.from.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const newVersion = values.to;

replaceOne(
  "package.json",
  new RegExp(`^(  "version": )"${oldVersion}",$`, "gm"),
  `$1"${newVersion}",`,
);
replaceOne(
  "package-lock.json",
  new RegExp(`^(  "version": )"${oldVersion}",$`, "gm"),
  `$1"${newVersion}",`,
);
replaceOne(
  "package-lock.json",
  new RegExp(
    `(^  "packages": \\{\\r?\\n    "": \\{\\r?\\n      "name": "shardx-launcher",\\r?\\n      "version": )"${oldVersion}"`,
    "gms",
  ),
  `$1"${newVersion}"`,
);
replaceOne(
  "src-tauri/tauri.conf.json",
  new RegExp(`^(  "version": )"${oldVersion}",$`, "gm"),
  `$1"${newVersion}",`,
);
replaceOne(
  "src-tauri/Cargo.toml",
  new RegExp(`^(version = )"${oldVersion}"$`, "gm"),
  `$1"${newVersion}"`,
);
replaceOne(
  "src-tauri/Cargo.lock",
  new RegExp(
    `(\\[\\[package\\]\\]\\r?\\nname = "shardx-launcher"\\r?\\nversion = )"${oldVersion}"`,
    "gms",
  ),
  `$1"${newVersion}"`,
);

console.log(`Updated release version from ${values.from} to ${values.to}`);
