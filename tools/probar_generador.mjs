// Prueba offline del generador móvil: extrae el <script id="core"> del HTML y lo compara con el vector de Rust.
// Uso: node tools/probar_generador.mjs
import fs from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";
const dir = new URL(".", import.meta.url).pathname;
const html = fs.readFileSync(dir + "generador_licencia_movil.html", "utf8");
const core = /<script id="core">([\s\S]*?)<\/script>/.exec(html)[1];
assert(!/https?:\/\//.test(html.replace(/<!--[\s\S]*?-->/g, "")), "el HTML no debe referenciar recursos externos");
const ctx = { module: { exports: {} }, globalThis: undefined, TextEncoder, TextDecoder, crypto: globalThis.crypto };
ctx.globalThis = ctx;
vm.runInNewContext(core, ctx);
const TC = ctx.module.exports;
const v = JSON.parse(fs.readFileSync(dir + "vector_firma.json", "utf8"));
const key = Uint8Array.from(v.keyfile_hex.match(/../g).map((h) => parseInt(h, 16)));
const seed = await TC.decryptKeyfile(key, v.password);
assert.equal(seed.length, 32);
await assert.rejects(TC.decryptKeyfile(key, "otra"), /Contraseña incorrecta/);
for (const c of v.cases) {
  const ch = TC.parseChallenge(c.req);
  const r = await TC.signChallenge(ch, c.days, v.now, seed);
  assert.equal(r.response, c.resp, "firma distinta a la de Rust para " + c.req);
}
// Tolerancia a minúsculas/espacios y detección de errores de copia
TC.parseChallenge(" " + v.cases[1].req.toLowerCase().replace(/-/g, " - ") + " ");
const typo = v.cases[0].req.replace(/.$/, (c) => (c === "A" ? "B" : "A"));
assert.throws(() => TC.parseChallenge(typo), /control/);
console.log("OK: " + v.cases.length + " firmas idénticas a Rust");
