// Fink JS interop — host-agnostic wrapper around a compiled fink WASM
// module (target: wasm+js). Uses only web-platform APIs (WebAssembly,
// TextEncoder/TextDecoder, Promise, Proxy) so it runs in browsers,
// Node, Deno, Bun, etc. without modification.
//
// Public API:
//   const fink = await init_wasm(bytes);
//   const mod  = await fink.import('module-name');
//   // navigate `mod` via JS Proxy: rec/seq access, fn apply...
//
// `init_wasm` is the single entry point; everything else (type_of,
// rec_get, apply, ...) lives behind the proxy returned by `import`.
//
// Internal style: helpers are module-scope builders that close over only
// what they're given (`(exports, ...deps) => fn`). `init_wasm` is the
// wiring point that instantiates the module and assembles the api.


// Private marker: every JS Proxy returned from `wrap` carries the
// underlying fink ref under this key. `to_fink` reads it back so a
// proxied value can be passed back into wasm as the original ref
// (no re-marshalling). User code never names this symbol.
const FINK_REF = Symbol('fink_ref');

const text_encoder = new TextEncoder();
const text_decoder = new TextDecoder();


// type_of tag values match js/interop.wat:type_of:
//   100 = Fn, 200 = Num, 220 = Int, 250 = Float, 300 = Bool,
//   400 = List, 500 = Rec, 600 = Str, 0 = Other.
const make_type_of = (exports) => (v) => {
  switch (exports.type_of(v)) {
    case 100: return 'Fn';
    case 200: return 'Num';
    case 220: return 'Int';
    case 250: return 'Float';
    case 300: return 'Bool';
    case 400: return 'List';
    case 500: return 'Rec';
    case 600: return 'Str';
    case 0:   return 'Other';
    default:  return `Unknown(${exports.type_of(v)})`;
  }
};


// Copy a JS string's UTF-8 bytes into wasm linear memory at the shared
// SCRATCH_BASE offset and return a fresh $ByteArray ref (a GC value).
// The linear-memory window can be reused on the next call — bytes
// already live in the GC heap. We avoid offset 0 because the user
// fragment's data segments (string literals) live there and would be
// clobbered by writes here.
const make_fink_bytes = (exports, scratch) => (s) => {
  const bytes = text_encoder.encode(s);
  new Uint8Array(exports.memory.buffer).set(bytes, scratch);
  return exports.bytes_from_js(scratch, bytes.length);
};


// Build a $Str ref from a JS string. Same dance as fink_bytes but
// wraps the result with $Str.
const make_str_from_js = (exports, scratch) => (s) => {
  const bytes = text_encoder.encode(s);
  new Uint8Array(exports.memory.buffer).set(bytes, scratch);
  return exports.str_from_js(scratch, bytes.length);
};


// Decode a $Str ref back into a JS string by writing its bytes into
// the SCRATCH_BASE window, then reading them via TextDecoder.
const make_str_to_js = (exports, scratch) => (s) => {
  const len = exports.str_to_js(s, scratch);
  return text_decoder.decode(new Uint8Array(exports.memory.buffer, scratch, len));
};


// Yield each element of a fink $List, walked via list_head/list_tail
// for `list_size` steps. Walking via the size bound avoids touching
// tails past the end, so list-impl details (Cons/Nil) never reach JS.
// Yields wrapped values so chained access works.
const make_list_iter = (exports, {wrap}) => function* list_iter(ref) {
  const n = exports.list_size(ref);
  let cur = ref;
  for (let i = 0; i < n; i++) {
    yield wrap(exports.list_head(cur));
    cur = exports.list_tail(cur);
  }
};


// Walk a $List and return the i-th element (wrapped). O(n).
const make_list_at = (exports, {wrap}) => (ref, i) => {
  let cur = ref;
  for (let k = 0; k < i; k++) cur = exports.list_tail(cur);
  return wrap(exports.list_head(cur));
};


const make_list_proxy = (exports, {list_iter, list_at}) => (ref) =>
  new Proxy({}, {
    get(_t, prop) {
      if (prop === FINK_REF)          return ref;
      if (prop === 'length')          return exports.list_size(ref);
      if (prop === Symbol.iterator)   return () => list_iter(ref);
      const i = Number(prop);
      if (Number.isInteger(i) && i >= 0 && i < exports.list_size(ref)) {
        return list_at(ref, i);
      }
      return undefined;
    },
  });


const make_rec_proxy = (exports, {wrap, str_from_js}) => (ref) =>
  new Proxy({}, {
    get(_t, prop) {
      if (prop === FINK_REF) return ref;
      if (typeof prop !== 'string') return undefined;
      const val = exports.rec_get(ref, str_from_js(prop));
      return val == null ? undefined : wrap(val);
    },
  });


// Marshal a JS value back into a fink ref. Proxied values short-circuit
// via FINK_REF (returns the underlying ref unchanged); primitives get
// marshalled into fresh fink values.
const make_to_fink = (exports, {str_from_js}) => (v) => {
  if (v != null && typeof v === 'object' && v[FINK_REF] !== undefined) {
    return v[FINK_REF];
  }
  if (typeof v === 'function' && v[FINK_REF] !== undefined) {
    return v[FINK_REF];
  }
  if (typeof v === 'number')  return exports.num_from_js(v);
  if (typeof v === 'boolean') return exports.i31_from_js(v ? 1 : 0);
  if (typeof v === 'string')  return str_from_js(v);
  // TODO: array → $List, plain object → $Dict.
  throw new Error(`to_fink: cannot marshal ${typeof v}: ${v}`);
};


// Build a fink args list from a JS array. The wat-side apply runtime
// (rt/apply.wat) expects the cont to be the *first* args element
// (CPS calling convention), so callers must prepend it before user
// args. This helper builds just the user-visible portion.
const make_args_from_js = (exports, {to_fink}) => (jsArgs) =>
  jsArgs.reduceRight(
    (tail, arg) => exports.args_prepend(to_fink(arg), tail),
    exports.args_empty(),
  );


// Callable Proxy over a fink $Closure. Apply trap marshals JS args,
// prepends a host-cont, and tail-calls the runtime's apply. The cont
// resolves a Promise on completion.
const make_fn_proxy = (exports, {wrap, args_from_js, ctx_state}) => (ref) => {
  const target = () => {};
  target[FINK_REF] = ref;
  return new Proxy(target, {
    get(_t, prop) {
      if (prop === FINK_REF) return ref;
      return undefined;
    },
    apply(_t, _this, args) {
      return new Promise((resolve, _reject) => {
        // CPS: the cont fires with a single-element args list whose
        // head is the function's result. Extract via list_head.
        const cont = (result) => resolve(wrap(exports.list_head(result)));
        const cont_ref = exports.wrap_host_cont_3(cont);
        const fink_args = exports.args_prepend(cont_ref, args_from_js(args));
        // Apply against the universe ctx captured at import time (carries
        // seeded effect slots, e.g. std/io.fnk's provider) so effectful
        // fink fns called from JS see the live universe. Falls back to a
        // fresh empty ctx if nothing has been imported yet.
        const ctx = ctx_state.current ?? exports.empty_ctx();
        exports.apply_3(fink_args, ctx, ref);
      });
    },
  });
};


// Generic ref → JS value wrapper. Primitive fink values unwrap to JS
// primitives; Lists / Recs stay as Proxy views; Closures become
// callable Proxies; everything else stays as the raw ref so the
// caller can keep digging. Mutually recursive with the proxy/converter
// builders, so we resolve the cycle by handing it a deps object the
// caller mutates after construction.
const make_wrap = (exports, deps) => (ref) => {
  if (ref == null) return undefined;
  switch (deps.type_of(ref)) {
    case 'Str':   return deps.str_to_js(ref);
    case 'List':  return deps.list_proxy(ref);
    case 'Rec':   return deps.rec_proxy(ref);
    case 'Fn':    return deps.fn_proxy(ref);
    case 'Num':
    case 'Int':
    case 'Float': return exports.num_to_js(ref);
    case 'Bool':  return !!exports.i31_to_js(ref);
    default:      return ref;
  }
};


// `import('./mod.fnk')` — call the per-module wrapper export with a
// host cont. The wrapper takes one arg: an opaque cont (anyref). When
// init_module finishes, it tail-applies the cont with two values —
// last_expr and the full exports rec (CPS args order: head =
// last_expr, tail.head = exports). Hosts that want a named export do
// their own rec_get against the exports rec.
const make_import = (exports, {wrap, ctx_state}) => (name) =>
  new Promise((resolve, _reject) => {
    const cont = (args, ctx) => {
      // Capture the module body's post-init ctx (seeded effect slots)
      // so later fn-proxy calls run against the live universe.
      if (ctx != null) ctx_state.current = ctx;
      const last_expr = exports.list_head(args);
      const tail      = exports.list_tail(args);
      const rec       = exports.list_head(tail);
      resolve([wrap(last_expr), wrap(rec)]);
    };
    const cont_ref = exports.wrap_host_cont_3(cont);
    exports[name](cont_ref);
  });


// Default host functions — keep them at module scope so the docstring's
// `init_wasm(bytes)` form (no opts) still works as before.
const default_host = {
  stdout_write: (s) => console.log(s),
  stderr_write: (s) => console.error(s),
  panic:        () => { throw new Error('host_panic'); },
};

export const init_wasm = async (bytes, host = {}) => {
  // The wat-side wrap_host_cont takes an externref handle and stores
  // it inside a $Closure-shaped cont (boxed in $Captures via
  // $ExternBox). When fink fires the cont via _apply, the adapter
  // pulls the externref back out and calls host_invoke_cont(handle,
  // args). We hand JS *functions* in as handles, so dispatch is
  // a single call — no id table, no map.
  //
  // `host` lets the caller redirect stdout/stderr/panic — tests
  // capture into arrays; the playground routes into a text field;
  // a CLI host writes to process.std{out,err}. Defaults route to the
  // global `console` and throw on panic.
  const { stdout_write, stderr_write, panic } = { ...default_host, ...host };

  // Forward decl — host_write reads from linear memory, so it needs
  // `exports` (set after instantiation). Fill it in below.
  let exports;
  const host_write = (fd_ref, ptr, len) => {
    // fd_ref is a fink $Num (heap-boxed); unwrap to a JS number.
    // (1=stdout, 2=stderr per std/io.fnk conventions.)
    const fd = exports.num_to_js(fd_ref);
    const bytes = new Uint8Array(exports.memory.buffer, ptr, len);
    const text = text_decoder.decode(bytes);
    if (fd === 2) stderr_write(text);
    else           stdout_write(text);
  };

  const env = {
    host_panic:        panic,
    host_yield:        (_resume, _ctx) => {},
    host_write,
    host_read_sync:    (_fd, _size, _ptr) => 0,
    host_invoke_cont:  (resolver, args, ctx) => resolver(args, ctx),
    // Trace source-line resolution needs the compiled debug marks, which
    // the wasm+js artifact does not carry. Return 0 (the import's
    // "unknown" sentinel) so get_loc yields no line in the JS host.
    host_resolve_loc:  (_mid, _cid) => 0,
  };

  const { instance } = await WebAssembly.instantiate(bytes, { env });
  exports = instance.exports;
  const scratch = exports.SCRATCH_BASE.value;

  // Build helpers. `wrap` is mutually recursive with the proxy
  // builders (a list/rec/fn proxy yields wrapped children), so we
  // create a single `deps` object and fill it in topological order
  // — `wrap` reads through `deps.*` at call time, not at build time.
  const deps = {};
  // Holds the universe ctx captured at import time (seeded effect
  // slots); fn-proxy applies read it so effectful fink fns called from
  // JS see the live universe. Null until the first import.
  deps.ctx_state    = { current: null };
  deps.type_of      = make_type_of(exports);
  deps.fink_bytes   = make_fink_bytes(exports, scratch);
  deps.str_from_js  = make_str_from_js(exports, scratch);
  deps.str_to_js    = make_str_to_js(exports, scratch);
  deps.wrap         = make_wrap(exports, deps);
  deps.list_iter    = make_list_iter(exports, deps);
  deps.list_at      = make_list_at(exports, deps);
  deps.list_proxy   = make_list_proxy(exports, deps);
  deps.rec_proxy    = make_rec_proxy(exports, deps);
  deps.to_fink      = make_to_fink(exports, deps);
  deps.args_from_js = make_args_from_js(exports, deps);
  deps.fn_proxy     = make_fn_proxy(exports, deps);

  return {
    import: make_import(exports, deps),
    // Raw helpers for testing / low-level inspection. Will be hidden
    // behind Proxy wrappers later.
    type_of:    deps.type_of,
    fink_bytes: deps.fink_bytes,
    str_from_js: deps.str_from_js,
    str_to_js:   deps.str_to_js,
    list_head:   exports.list_head,
    list_tail:   exports.list_tail,
    list_size:   exports.list_size,
    list_iter:   deps.list_iter,
    rec_get:     exports.rec_get,
    wrap:        deps.wrap,
    to_fink:     deps.to_fink,
  };
};
