import assert from "node:assert/strict";
import fs from "node:fs";
import ts from "typescript";

// Execute the actual editor conversion functions without booting a WebView.
const source = fs.readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
const ast = ts.createSourceFile("App.tsx", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const names = new Set(["defaultForm", "fromStored", "toStored", "deriveAcceptLanguage", "deriveLanguagesArray", "DEFAULT_BLOCKED_PORTS", "AUTO_TZ", "AUTO_LANG", "WEBGL_NOISE_INTENSITY", "CLIENT_RECTS_MAX_OFFSET"]);
const declarations = ast.statements.filter(node => (node.name && names.has(node.name.text)) || (ts.isVariableStatement(node) && node.declarationList.declarations.some(d => names.has(d.name.getText(ast))))).map(node => node.getText(ast)).join("\n");
const code = ts.transpileModule(declarations, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
const { fromStored, toStored } = new Function(`${code}; return {fromStored,toStored}`)();
const mergeCode = ts.transpileModule(fs.readFileSync(new URL("../src/profileEdit.ts", import.meta.url), "utf8"), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText;
const exports = {};
new Function("exports", mergeCode)(exports);
const { mergeProfileEdits } = exports;

const original = {
  _meta: { id: "identity", gpu_preset_id: "preset", extensions: ["extension"], proxy_id: "proxy" }, _revision: "original",
  name: "Original", notes: "before", timezone: "Asia/Shanghai", icu_locale: "zh-Hans-CN",
  navigator: { language: "zh-CN", languages: ["zh-CN", "en-US", "fr"], accept_language: "zh-CN,en-US;q=0.7,fr;q=0.4", do_not_track: "0", platform_version: "17.0.0" },
  screen: { width: 1200 }, window: { width: 1000 },
  noise: { canvas: { enabled: true, seed: 9183, extra: "preserve" } },
};
const form = fromStored(original);
assert.equal(form.do_not_track, false);
assert.equal(form.platform_version, "17.0.0");
const edited = toStored({ ...form, name: "Renamed" }, null, original, form);
assert.deepEqual(edited.navigator, original.navigator);
assert.deepEqual(edited.noise, original.noise);
assert.deepEqual(edited.screen, original.screen);
assert.deepEqual(edited.window, original.window);
assert.equal(edited.icu_locale, original.icu_locale);
const latest = structuredClone(original);
latest.notes = "external"; latest._meta.extensions = ["other-extension"]; latest._revision = "latest";
const merged = mergeProfileEdits(original, edited, latest);
assert.equal(merged.name, "Renamed"); assert.equal(merged.notes, "external");
assert.deepEqual(merged._meta.extensions, latest._meta.extensions); assert.equal(merged._revision, "latest");
assert.throws(() => mergeProfileEdits(original, edited, { ...latest, name: "Concurrent" }), /name/);
const enabled = toStored({ ...form, do_not_track: true }, null, original, form);
assert.equal(enabled.navigator.do_not_track, "1");
assert.deepEqual(enabled.navigator.languages, original.navigator.languages);
console.log("Editor regression passed: language/DNT preservation, three-way merge, conflict detection, and identity retention.");
