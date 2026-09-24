"""python3 -m unittest  (run from scripts/nostos_rename/)"""

import unittest

from rules import rename_content, rename_path, rename_text, scan


def rc(text, path="x.rs"):
    return rename_content(path, text.encode()).decode()


def classes(text, path=None):
    return {c for _, _, c, _ in scan(path, text) if c}


class Renames(unittest.TestCase):
    def test_case_variants(self):
        self.assertEqual(
            rc("cairn Cairn CAIRN CairnClient CAIRN_SYNC_URL cairn_core cairn-server"),
            "nostos Nostos NOSTOS NostosClient NOSTOS_SYNC_URL nostos_core nostos-server")

    def test_namespaces(self):
        self.assertEqual(rc("dev.cairn.cairn_flutter com.cairn.sdk dev.cairn.app"),
                         "run.nostos.nostos_flutter run.nostos.sdk run.nostos.app")
        self.assertEqual(rc("kotlin/com/cairn/reactnative/"), "kotlin/run/nostos/reactnative/")

    def test_npm_scope_domain_email(self):
        self.assertEqual(rc('from "@cairn/web"; founders@cairn.dev https://cairn.dev/docs'),
                         'from "@nostos-sync/web"; founders@nostos.run https://nostos.run/docs')

    def test_homebrew(self):
        self.assertEqual(rc("brew tap unfazed-dev/cairn; brew install unfazed-dev/cairn/cairn; homebrew-cairn"),
                         "brew tap unfazed-dev/tap; brew install unfazed-dev/tap/nostos; homebrew-tap")

    def test_push_crate_vs_edge_function(self):
        self.assertEqual(rc("cargo run -p cairn-push; supabase functions deploy cairn-push"),
                         "cargo run -p nostos-push; supabase functions deploy cairn-push")
        self.assertEqual(rc("the `cairn-push` Edge Function"), "the `cairn-push` Edge Function")
        fn = "supabase/functions/cairn-push/index.ts"  # dir held, source renamed (ADR-0046 fallback)
        self.assertEqual(rename_path(fn), fn)
        self.assertEqual(rc('Deno.env.get("CAIRN_PUSH_SECRET"); data: { cairn: "ring" }', fn),
                         'Deno.env.get("NOSTOS_PUSH_SECRET"); data: { cairn: "ring" }')

    def test_dsn(self):
        held = "postgres://cairn:cairn@localhost:5433/cairn"
        self.assertEqual(rc(held), held)
        self.assertEqual(rc("postgres://cairn:cairn@cairn-postgres:5432/cairn\n"),
                         "postgres://cairn:cairn@nostos-postgres:5432/cairn\n")
        self.assertEqual(rc("postgres://cairn:${CAIRN_PG_PASSWORD}@localhost/cairn"),
                         "postgres://cairn:${NOSTOS_PG_PASSWORD}@localhost/cairn")
        self.assertEqual(rc("postgres://h/cairn_x"), "postgres://h/nostos_x")


class Holds(unittest.TestCase):
    # (class, original path or None, text that must survive verbatim)
    CASES = [
        ("metric", None, "cairn_events_delivered_total cairn_slot_epoch cairn_push_"),
        ("pg-object", None, "cairn_pull cairn_oplog cairn_log_tasks cairn_writer cairn\\\\_log"),
        ("pg-schema", None, "cairn.changes; create schema if not exists cairn; the `cairn` schema"),
        ("push-payload", None, "{cairn: 'ring'} data['cairn'] == 'ring' cairn_route"),
        ("push-channel", None, 'CHANNEL_ID = "cairn"'),
        ("pg-sql-literal", None, "'cairn' || 'cairn:'"),
        ("pg-credential", None, "POSTGRES_USER: cairn\npsql -U cairn -d cairn\ncreate ROLE cairn"),
        ("pg-dsn", None, "postgres://cairn:cairn@localhost/cairn"),
        ("client-storage", None, "cairn_outbox cairn.db cairn_direct.sqlite cairn-pushd.db "
                                 "cairn:opfs-sahpool cairn:checkpoint:t cairn:experimental:x"),
        ("wire", None, "realtime:cairn:t cairn/sync/1 cairn:multitab X-Cairn-Source `cairn:`"),
        ("cloud-cookie", None, "cairn_session"),
        ("tauri-plugin", None, 'tauri-plugin-cairn tauri_plugin_cairn plugin:cairn|x plugins.cairn '
                               'Builder::new("cairn") cairn:default cairn:allow-query'),
        ("tauri-plugin", "sdk/cairn_tauri/fixture/tauri.conf.json", '"plugins": {"cairn": {}}'),
        ("edge-function", None, 'functions/v1/cairn-push functions deploy cairn-push "deploy",\n "cairn-push"'),
        ("archive", None, "cairn-archive"),
        ("git-pin", None, "unfazed-dev/cairn#a1b2c3d4 unfazed-dev/cairn.git?rev=0123abcd"),
        ("brand-metaphor", None, "A cairn is a pile of stones. Cairns mark trails."),
        ("legal-entity", None, "Cairn Sync, Inc."),
        ("atlet-engine-id", "apps/atlet/flutter/lib/x.dart",
         "enum Engine { cairn, cairnDirect } Engine.cairn 'cairnDirect' 'cairn/tasks' `cairnDirect`"),
        ("wasm-bindgen-symbol", "apps/atlet/flutter/web/cairn/cairn_ffi_wasm.js",
         "wasm.cairnsocket_new __wbg_cairnengine_x import './cairn_ffi_wasm_bg.js'"),
    ]

    def test_every_class(self):
        for cls, path, text in self.CASES:
            with self.subTest(cls=cls, text=text):
                self.assertEqual(rename_content(path or "x.rs", text.encode()).decode(), text)
                self.assertIn(cls, classes(text, path))

    def test_scoped_holds_stay_scoped(self):
        self.assertEqual(rc("Engine.cairn 'cairn'", "sdk/x.dart"), "Engine.nostos 'cairn'")
        self.assertEqual(rc("wasm.cairnsocket_new", "sdk/x.js"), "wasm.nostossocket_new")

    def test_holds_do_not_stop_neighbours(self):
        self.assertEqual(rc("cairn_pull via CairnClient"), "cairn_pull via NostosClient")

    def test_marker(self):
        src = 'const LEGACY = "CAIRN_"; // rename:hold\nlet cairn = 1;\n'
        self.assertEqual(rc(src), 'const LEGACY = "CAIRN_"; // rename:hold\nlet nostos = 1;\n')


class Blobs(unittest.TestCase):
    def test_binary_skipped(self):
        blob = b"cairn\0Cairn"
        self.assertIs(rename_content("a/cairn.wasm", blob), blob)

    def test_held_paths(self):
        for p in ["supabase/migrations/0001_init.sql", "apps/atlet/supabase/migrations/0002.sql",
                  "docs/ci/decisions.md",
                  "docs/plans/nostos-name-map-2026-09-24.md", "scripts/nostos_rename/rules.py"]:
            with self.subTest(p=p):
                self.assertEqual(rename_path(p), p)
                self.assertEqual(rename_content(p, b"cairn Cairn"), b"cairn Cairn")

    def test_paths(self):
        self.assertEqual(rename_path("crates/cairn-core/src/lib.rs"), "crates/nostos-core/src/lib.rs")
        self.assertEqual(rename_path("sdk/cairn_kotlin/android/src/main/java/com/cairn/sdk/CairnClient.kt"),
                         "sdk/nostos_kotlin/android/src/main/java/run/nostos/sdk/NostosClient.kt")
        self.assertEqual(rename_path("apps/atlet/flutter/web/cairn/cairn_ffi_wasm_bg.wasm"),
                         "apps/atlet/flutter/web/nostos/nostos_ffi_wasm_bg.wasm")
        self.assertEqual(rename_path("README.md"), "README.md")

    def test_cargo_lock_resorted(self):
        lock = (
            'version = 4\n\n'
            '[[package]]\nname = "cairn-core"\nversion = "0.1.0"\ndependencies = [\n'
            ' "cairn-domain",\n "memchr",\n "nom",\n]\n\n'
            '[[package]]\nname = "cairn-domain"\nversion = "0.1.0"\n\n'
            '[[package]]\nname = "memchr"\nversion = "2.7.4"\n\n'
            '[[package]]\nname = "nom"\nversion = "7.1.3"\n\n'
            '[[package]]\nname = "zerocopy"\nversion = "0.8.0"\n')
        self.assertEqual(rc(lock, "sdk/x/Cargo.lock"), (
            'version = 4\n\n'
            '[[package]]\nname = "memchr"\nversion = "2.7.4"\n\n'
            '[[package]]\nname = "nom"\nversion = "7.1.3"\n\n'
            '[[package]]\nname = "nostos-core"\nversion = "0.1.0"\ndependencies = [\n'
            ' "memchr",\n "nom",\n "nostos-domain",\n]\n\n'
            '[[package]]\nname = "nostos-domain"\nversion = "0.1.0"\n\n'
            '[[package]]\nname = "zerocopy"\nversion = "0.8.0"\n'))


class Facts(unittest.TestCase):
    SAMPLES = [("crates/cairn-core/src/lib.rs", "use cairn_domain::Lsn; // cairn_pull, `cairn:` topic\n"),
               ("apps/atlet/flutter/web/cairn/cairn_ffi_wasm.js", "wasm.cairnsocket_new(); CairnSocket\n"),
               ("sdk/cairn_tauri/tauri.conf.json", '{"plugins": {"cairn": {}}, "id": "dev.cairn.x"}\n')]

    def test_idempotent(self):
        for path, text in self.SAMPLES:
            with self.subTest(path=path):
                once = rename_content(path, text.encode())
                self.assertEqual(rename_content(rename_path(path), once), once)
                self.assertEqual(rename_path(rename_path(path)), rename_path(path))
        msg = "feat(cairn-core): Cairn reads cairn_pull"
        self.assertEqual(rename_text(rename_text(msg)), rename_text(msg))

    def test_output_has_no_generic_survivor(self):
        self.assertNotIn("airn", rc("Cairn cairn CAIRN cairn_core CairnDatabase").lower())

    def test_pure(self):
        a = [rename_content(p, t.encode()) for p, t in self.SAMPLES]
        rename_content("x", b"cairn")  # unrelated call in between
        self.assertEqual([rename_content(p, t.encode()) for p, t in self.SAMPLES], a)

    def test_rename_text(self):
        self.assertEqual(rename_text("chore: bump Cairn, keep cairn_pull"), "chore: bump Nostos, keep cairn_pull")
        self.assertEqual(rename_text("no match"), "no match")


if __name__ == "__main__":
    unittest.main()
