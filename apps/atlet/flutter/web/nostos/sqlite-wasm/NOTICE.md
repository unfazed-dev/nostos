This directory contains the browser runtime files `index.mjs` and
`sqlite3.wasm` from `@sqlite.org/sqlite-wasm` version `3.53.4-build1`.
The exact package version is locked by `sdk/nostos_web/package-lock.json`.
The npm package declares Apache-2.0; `LICENSE` contains the license text.

Regenerate with `npm ci` in `sdk/nostos_web`, then copy those two files from
`node_modules/@sqlite.org/sqlite-wasm/dist/` when updating the locked version.
