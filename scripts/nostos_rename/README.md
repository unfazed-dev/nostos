# nostos_rename

The mechanical cairn → nostos rename. `rules.py` is a set of pure functions of
(original path, bytes). The same functions forward-rename a tree (`apply.py`)
and rewrite history (git-filter-repo), so the rewritten tip is byte-identical
to the forward rename.

- `rename_path(path)`: new repo-relative path.
- `rename_content(path, data)`: new bytes. `path` is the ORIGINAL path.
  Binary blobs (NUL in the first 8000 bytes) and held paths come back unchanged.
- `rename_text(text)`: commit and tag messages.

Generic rule: `cairn`/`Cairn`/`CAIRN` → `nostos`/`Nostos`/`NOSTOS`.

Specific renames:

| from | to |
|---|---|
| `dev.cairn` / `com.cairn` (and as dirs) | `run.nostos` |
| `@cairn` | `@nostos-sync` |
| `cairn.dev` | `nostos.run` |
| `homebrew-cairn` | `homebrew-tap` |

Everything else is left exactly as it was: the held identifiers (bucket A,
decision 2b in `docs/ci/decisions.md`).

## Hold classes

| class | what stays `cairn` |
|---|---|
| `metric` | Prometheus names `cairn_*_total`, … |
| `pg-object`, `pg-schema`, `pg-sql-literal` | Postgres functions, tables, triggers, publication/slot, schema `cairn.*`, SQL literals `'cairn'`/`'cairn:…'` |
| `pg-credential`, `pg-dsn` | db, user and password `cairn` (in env vars, `-U`/`-d`, roles, the user/password/db parts of `postgres://` URLs, and prose naming the `cairn` superuser) |
| `client-storage` | on-device SQLite files and tables, OPFS pool, checkpoint and localStorage keys |
| `wire` | the Realtime topic `realtime:cairn:`, ALPN `cairn/sync/1`, `cairn:multitab`, `X-Cairn-Source` |
| `push-payload`, `push-channel` | the `{cairn:'ring'}` payload, `cairn_route`, the Android channel id |
| `cloud-cookie` | `cairn_session` |
| `tauri-plugin` | crate `tauri-plugin-cairn`, plugin id, ACL ids, `plugins.cairn`. Tauri derives the ACL namespace from the crate name. |
| `edge-function` | the deployed Supabase function `cairn-push` and its `supabase/functions/cairn-push/` dir. Its source renames like any other file. |
| `atlet-engine-id` | atlet's persisted `Engine.cairn`/`cairnDirect`: the enum anywhere, the string ids under `apps/atlet/` only |
| `wasm-bindgen-symbol` | symbols baked into the committed `.wasm`, in its glue JS only |
| `archive`, `git-pin`, `brand-metaphor`, `legal-entity` | `cairn-archive`, SHA-pinned git URLs, the word cairn as a noun, `Cairn Sync, Inc.` |
| `held-path:*` | files kept byte-for-byte: `supabase/migrations/`, the rename docs, this directory |
| `marker` | any line containing `rename:hold` |

## Run

```sh
cd scripts/nostos_rename
python3 -m unittest
python3 apply.py HEAD /tmp/renamed       # or a working-tree dir instead of a ref
python3 census.py HEAD /tmp/renamed      # exits 1 on any unexplained survivor, or a migration-held
                                         # name that is renamed elsewhere
```

History. Run this on a fresh clone, never on the working clone. `R` must be
an absolute path to this directory:

- `--file-info-callback` sees the original path, so it can pass it to
  `rename_content`.
- Deletions never reach that callback, so the commit callback renames their
  paths.

```sh
R=/abs/path/to/scripts/nostos_rename
git filter-repo --force \
  --file-info-callback "
import sys; sys.path.insert(0, '$R'); import rules
u = lambda b: b.decode('utf-8', 'surrogateescape')
name, data = u(filename), value.get_contents_by_identifier(blob_id)
out = rules.rename_path(u(data)).encode('utf-8', 'surrogateescape') if mode == b'120000' \
    else rules.rename_content(name, data)
if out != data:
    blob_id = value.insert_file_with_contents(out)
return (rules.rename_path(name).encode('utf-8', 'surrogateescape'), mode, blob_id)" \
  --commit-callback "
import sys; sys.path.insert(0, '$R'); import rules
for c in commit.file_changes:
    if c.type == b'D':
        c.filename = rules.rename_path(c.filename.decode('utf-8', 'surrogateescape')).encode('utf-8', 'surrogateescape')" \
  --message-callback "
import sys; sys.path.insert(0, '$R'); import rules
return rules.rename_text(message.decode('utf-8', 'surrogateescape')).encode('utf-8', 'surrogateescape')"
```

Then `git rev-parse HEAD^{tree}` must equal the tree of the `apply.py` output
for the same commit.

After the rename, run `cargo fmt --all` and `dart format`. Renamed identifiers
are one character longer and sort differently, so the formatters rewrap
lines and reorder imports. Commit that as a separate commit. Then regenerate
`sdk/nostos_dotnet/dotnet/generated/nostos.cs` (its README, "Build"): the
uniffi checksums hash the renamed symbols, and only those lines change.
`Cargo.lock` files are re-sorted here; `package-lock.json` keys are not, so
the next `npm install` moves `@nostos-sync/*` entries (`npm ci` accepts the
file as is).
