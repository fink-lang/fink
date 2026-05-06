// Node test harness for the Fink JS interop layer.
//
// Driven by src/runtime/interop/js/test_interop.rs, which compiles
// test_interop.fnk with --target=wasm+js (linking js/interop.wat) and
// invokes `node --test` on this file with the wasm path supplied via
// FINK_TEST_WASM.

import { before, test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

import { init_wasm } from './fink.js';


const wasm_path = process.env.FINK_TEST_WASM;
if (!wasm_path) {
  throw new Error('FINK_TEST_WASM env var not set');
}

let fink;

before(async () => {
  const bytes = await readFile(wasm_path);
  fink = await init_wasm(bytes);
});

test('init_wasm yields a fink object with import', () => {
  assert.equal(typeof fink.import, 'function');
});

test('import returns the entry module', async () => {
  const mod = await fink.import('./test_interop.fnk');
  assert.ok(mod);
});

test('str round-trip: js -> $Str -> js', () => {
  const s = fink.str_from_js('hello world');
  assert.equal(fink.type_of(s), 'Str');
  assert.equal(fink.str_to_js(s), 'hello world');
});

test('str round-trip: utf-8 multibyte', () => {
  const s = fink.str_from_js('résumé €');
  assert.equal(fink.str_to_js(s), 'résumé €');
});


test('apply a fink fn from JS', async () => {
  const [last_val, {foo}] = await fink.import('./test_interop.fnk');
  assert.equal(last_val, 42);
  const result = await foo(2, 3);
  assert.equal(result, 5);
});

test('bool round-trip via identity fn', async () => {
  const [, {bar}] = await fink.import('./test_interop.fnk');
  assert.equal(await bar(true), true);
  assert.equal(await bar(false), false);
});
