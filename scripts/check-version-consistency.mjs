import { parseArgs } from "node:util";
import { readFileSync } from "node:fs";

const VERSION_PATTERN = "(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)";
const VERSION_RE = new RegExp(`^${VERSION_PATTERN}$`);
const TAG_RE = new RegExp(`^v${VERSION_PATTERN}$`);

function fail(message) {
  console.error(`Version consistency check failed: ${message}`);
  process.exit(1);
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(`cannot parse ${path}: ${error.message}`);
  }
}

function readPackageVersion(path, name) {
  const content = readFileSync(path, "utf8");
  const packagePattern = new RegExp(
    `^\\[\\[package\\]\\]\\r?\\nname = "${name}"\\r?\\nversion = "(${VERSION_PATTERN})"`,
    "gm",
  );
  const matches = [...content.matchAll(packagePattern)];
  if (matches.length !== 1) {
    fail(`${path} must contain exactly one package entry for ${name}; found ${matches.length}`);
  }
  return matches[0][1];
}

function readTomlPackageVersion(path, name) {
  const content = readFileSync(path, "utf8");
  const packageSection = content.match(/^\[package\]\r?\n([\s\S]*?)(?=^\[[^\]]+\]\s*$)/m);
  if (!packageSection) {
    fail(`${path} has no [package] section`);
  }

  const nameMatch = packageSection[1].match(/^name\s*=\s*"([^"]+)"\s*$/m);
  const versionMatch = packageSection[1].match(new RegExp(`^version\\s*=\\s*"(${VERSION_PATTERN})"\\s*$`, "m"));
  if (nameMatch?.[1] !== name || !versionMatch) {
    fail(`${path} must declare package ${name} with a canonical version`);
  }
  return versionMatch[1];
}

const { values, positionals } = parseArgs({
  options: {
    tag: { type: "string" },
  },
  strict: true,
  allowPositionals: true,
});

if (positionals.length > 0) {
  fail(`unexpected positional arguments: ${positionals.join(" ")}`);
}
if (values.tag && !TAG_RE.test(values.tag)) {
  fail(`invalid tag ${JSON.stringify(values.tag)}; expected vMAJOR.MINOR.PATCH`);
}

const packageJson = readJson("package.json");
const packageLock = readJson("package-lock.json");
const tauriConfig = readJson("src-tauri/tauri.conf.json");
const versions = {
  "package.json": packageJson.version,
  "package-lock.json": packageLock.version,
  "package-lock.json packages[\"\"]": packageLock.packages?.[""]?.version,
  "src-tauri/Cargo.toml": readTomlPackageVersion("src-tauri/Cargo.toml", "shardx-launcher"),
  "src-tauri/Cargo.lock": readPackageVersion("src-tauri/Cargo.lock", "shardx-launcher"),
  "src-tauri/tauri.conf.json": tauriConfig.version,
};

for (const [source, version] of Object.entries(versions)) {
  if (typeof version !== "string" || !VERSION_RE.test(version)) {
    fail(`${source} has invalid version ${JSON.stringify(version)}`);
  }
}

const sourceVersion = versions["package.json"];
const mismatches = Object.entries(versions)
  .filter(([, version]) => version !== sourceVersion)
  .map(([source, version]) => `${source}=${version}`);
if (mismatches.length > 0) {
  fail(`source version is ${sourceVersion}, but ${mismatches.join(", ")}`);
}

if (values.tag && values.tag.slice(1) !== sourceVersion) {
  fail(`tag ${values.tag} does not exactly match source version v${sourceVersion}`);
}

console.log(`Version consistency check passed: v${sourceVersion}`);
