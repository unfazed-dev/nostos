// Guards the packaging invariant that is silent when broken: both wasm cores
// must be inside the published tarball.
//
// wasm-pack writes `pkg-web/.gitignore` containing `*`, and npm falls back to a
// directory's .gitignore when that directory has no .npmignore. The result was a
// @nostos-sync/web tarball with every JS file and no engine — it installs fine and
// fails at runtime. The staging step drops an empty `.npmignore` in both pkg dirs;
// this asserts the outcome rather than the mechanism.
//
// Run: node check-pack.cjs   (from sdk/nostos_web)

const { execFileSync } = require("node:child_process");

const REQUIRED = [
  "pkg-node/nostos_ffi_wasm_bg.wasm",
  "pkg-node/nostos_ffi_wasm.js",
  "pkg-web/nostos_ffi_wasm_bg.wasm",
  "pkg-web/nostos_ffi_wasm.js",
  "worker/nostos.worker.js",
  "index.js",
];

const out = execFileSync("npm", ["pack", "--dry-run", "--json"], {
  encoding: "utf8",
  stdio: ["ignore", "pipe", "ignore"],
});
const files = JSON.parse(out)[0].files.map((f) => f.path);

const missing = REQUIRED.filter((r) => !files.includes(r));
if (missing.length) {
  console.error("MISSING from the @nostos-sync/web tarball:\n  " + missing.join("\n  "));
  console.error(`\n(${files.length} files packed; run \`npm run build:all\` first)`);
  process.exit(1);
}
console.log(`ok — ${files.length} files packed, wasm core present`);
