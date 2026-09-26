"use strict";

const fs = require("node:fs");
const path = require("node:path");

const target = process.argv[2];
if (target !== "node" && target !== "web") {
  throw new Error("usage: node stage-wasm.cjs node|web");
}

const pkg = `pkg-${target}`;
const source = path.resolve(__dirname, "../../crates/nostos-ffi-wasm", pkg);
const destination = path.join(__dirname, pkg);
if (!fs.existsSync(path.join(source, "nostos_ffi_wasm_bg.wasm"))) {
  throw new Error(`${pkg} is missing; run npm run build:${target === "node" ? "all" : "web"}`);
}

fs.rmSync(destination, { recursive: true, force: true });
fs.cpSync(source, destination, { recursive: true });
// wasm-pack writes a `*` .gitignore; npm must include the generated files.
fs.writeFileSync(path.join(destination, ".npmignore"), "");
