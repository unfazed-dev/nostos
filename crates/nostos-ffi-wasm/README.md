# nostos-ffi-wasm — the WebAssembly bridge

A thin `#[wasm_bindgen]` projection of the `nostos-core` apply engine for
JavaScript, plus the browser transport and the sqlite-wasm/OPFS storage
(ADR-0017, ADR-0033). No sync logic of its own. Consumed by `sdk/nostos_web`
and, through it, the Capacitor plugin and Flutter web (ADR-0036).

Depends on `nostos-core` and `nostos-domain`.

```sh
wasm-pack build crates/nostos-ffi-wasm --target web --out-dir pkg-web
make web-demo   # builds this, then serves the /demo page against make dev-stack
```
