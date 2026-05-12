// Smoke test for the wasm-pack `--target web` bundle running under Node.
//
// The bundle expects to be loaded with `fetch(url) -> Response`; under Node
// we hand the wasm bytes to `initSync` so we exercise the same module
// without the browser-only fetch path.
//
// Run:  node test-node.mjs   (after `wasm-pack build --target web
//                                 --out-dir ../../docs/.wasm/relon`)
//
// Exits non-zero on any failure; CI / `npm run build:wasm` can chain it.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { initSync, evaluate, format, version } from '../../docs/public/wasm/relon/relon_wasm.js';

const here = dirname(fileURLToPath(import.meta.url));
const wasmPath = resolve(here, '../../docs/public/wasm/relon/relon_wasm_bg.wasm');
initSync({ module: readFileSync(wasmPath) });

let failures = 0;
function check(label, cond, detail) {
    if (cond) {
        console.log(`  ok  ${label}`);
    } else {
        console.error(`  FAIL ${label}: ${detail ?? ''}`);
        failures += 1;
    }
}

console.log('version =', version());
check('version is a non-empty string', typeof version() === 'string' && version().length > 0);

// 1. happy path: object sources.
const out1 = evaluate({ 'main.relon': '{ price: 100 + 23 }' }, 'main.relon');
console.log('evaluate({price: 100+23}) =', JSON.stringify(out1));
check('object-sources arithmetic', out1 && out1.price === 123, JSON.stringify(out1));

// 2. array sources + cross-module import.
const out2 = evaluate(
    [
        { path: 'main.relon', content: '#import lib from "./lib.relon"\n{ g: lib.hello + ", world" }' },
        { path: 'lib.relon', content: '{ hello: "hi" }' },
    ],
    'main.relon'
);
console.log('cross-module evaluate =', JSON.stringify(out2));
check('cross-module import', out2 && out2.g === 'hi, world', JSON.stringify(out2));

// 3. parse error: surfaces as ErrorReport with ParseError kind.
try {
    evaluate({ 'main.relon': '{ not closed' }, 'main.relon');
    check('parse error throws', false, 'no throw');
} catch (err) {
    console.log('parse-error payload =', JSON.stringify(err));
    check('parse error kind', err && err.kind === 'ParseError', JSON.stringify(err));
    check('parse error message non-empty', err && typeof err.message === 'string' && err.message.length > 0);
}

// 4. missing entry: InvalidInput.
try {
    evaluate({ 'main.relon': '{ a: 1 }' }, 'missing.relon');
    check('missing entry throws', false);
} catch (err) {
    check('missing entry kind', err && err.kind === 'InvalidInput', JSON.stringify(err));
}

// 5. format passthrough.
const formatted = format('{a:1,b:2}');
console.log('format =', JSON.stringify(formatted));
check('format returns a string containing both keys', formatted.includes('a') && formatted.includes('b'));

if (failures > 0) {
    console.error(`\n${failures} check(s) failed.`);
    process.exit(1);
}
console.log('\nall wasm smoke checks passed.');
