import assert from "node:assert/strict";
import { defaultForm, fromStored, toStored } from "../src/profileForm.ts";
import { mergeProfileEdits } from "../src/profileEdit.ts";

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

const manualLanguage = toStored({ ...form, language: "fr-FR" }, null, original, form);
assert.equal(manualLanguage.icu_locale, "fr-FR");
assert.equal(manualLanguage.navigator.accept_language, "fr-FR,fr;q=0.9,en-US;q=0.8,en;q=0.7");
assert.deepEqual(manualLanguage.navigator.languages, ["fr-FR", "fr", "en-US", "en"]);
const autoLanguage = toStored({ ...form, language: "auto" }, null, original, form);
assert.equal(autoLanguage.icu_locale, null);
assert.equal(autoLanguage.navigator.languages, null);
assert.equal(autoLanguage.navigator.accept_language, null);
const template = { screen: { width: 1920 }, navigator: { vendor: "Template vendor" } };
const switched = toStored({ ...form, gpu_preset_id: "another-preset" }, { payload: template }, original, form);
assert.equal(switched._meta.id, "identity");
assert.equal(switched._meta.gpu_preset_id, "another-preset");
assert.deepEqual(switched.screen, template.screen);
assert.equal(switched.navigator.vendor, "Template vendor");
assert.equal(switched.noise.canvas.seed, 9183);
assert.deepEqual(template, { screen: { width: 1920 }, navigator: { vendor: "Template vendor" } });
const fresh = toStored(defaultForm(), null);
assert.equal(fresh._meta.id, "");
assert.equal(fresh.noise.canvas.seed, 0);
assert.equal(fresh.navigator.language, "auto");
console.log("Editor regression passed: create, edit, language/DNT, preset switch, identity, and concurrent edits.");
