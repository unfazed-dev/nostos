"""cairn -> nostos rename rules: one pure function of (original path, bytes).

Used twice: apply.py forward-renames a tree, and git-filter-repo calls the
same functions on every historical blob, path and message, so the rewritten
tip is byte-identical to the forward rename.

Bucket C (build-time names) is renamed; bucket A (persisted or on-the-wire
identity, decision 2b in docs/ci/decisions.md) is held. Each hold has a class
name; census.py counts survivors per class.
"""

import re

MARKER = "rename:hold"  # any line containing this is copied verbatim

# (class, regex on the ORIGINAL repo-relative posix path). Held paths keep
# both their path and their bytes.
HELD_PATHS = [
    ("held-path:migrations", r"(?:^|/)supabase/migrations/"),
    ("held-path:rename-docs",
     r"^docs/plans/(?:nostos-|qairn-rename-inventory-)[^/]*\.md$|^docs/ci/decisions\.md$"),
    ("held-path:rename-tool", r"^scripts/nostos_rename/"),
]

_NB = r"(?<![A-Za-z0-9])"  # snake-case names: `_` may precede (idx_cairn_…)
_W = r"(?<![\w-])"

# (class, regex). Matched text is copied verbatim. Order only decides which
# class a span is counted under; every hold beats every rename.
HOLDS = [
    ("metric", _NB + r"cairn_(?:\w+_total|live_sessions|push_last_lsn|replication_lag_bytes"
               r"|slot_(?:epoch|wal_status|lag_bytes))(?![A-Za-z0-9])"),
    ("metric", _NB + r"cairn_(?:events|oplog|push|slot|live)_(?![A-Za-z0-9])"),  # grep '^cairn_push_'
    ("pg-object", _NB + r"cairn_(?:pull|snapshot|increment|heartbeat|(?:de)?register_push_token"
                  r"|push_t(?:okens|argets)|changes_\w+|ring(?:_\w+)?|log_\w*"
                  r"|oplog(?:_\w+)?|pub(?:_\w+)?|slot(?:_\w+)?|writer(?:_\w+)?)(?![A-Za-z0-9])"),
    ("pg-object", r"cairn\\{1,2}_log"),  # like 'cairn\\_log\\_%' inside Rust strings
    ("pg-schema", r"(?<![\w@/.-])cairn\.(?=(?:changes|push_tokens|push_config|push_templates"
                  r"|push_cooldown|device_presence|current_scopes|log_change|retention|prune"
                  r"|ring|pull|wake_absent_devices)\b)"),
    ("pg-schema", r"(?i:\bschema)[\"']?:?\s+(?i:if\s+(?:not\s+)?exists\s+)?[\"'`]?cairn(?![\w-])"),
    ("pg-schema", _W + r"cairn(?=[`'\"]?\s+schema\b)"),
    ("push-payload", _W + r"['\"]?cairn(?=['\"]?\]?\s*(?::|==)\s*['\"]?ring\b)"),  # {cairn:'ring'}
    ("push-payload", _NB + r"cairn_route(?![A-Za-z0-9])"),  # notification tap-routing data key
    ("push-channel", r"(?:CHANNEL_ID|channel_id)[\w\s:&\"'=]{0,12}?cairn(?![\w-])"),  # Android channel
    ("pg-sql-literal", r"'cairn'|'cairn:"),  # schema name, realtime topic, lock key, error texts
    ("pg-credential", r"(?:\b(?:POSTGRES_(?:USER|PASSWORD|DB)|PG(?:USER|PASSWORD|DATABASE)"
                      r"|user|password|dbname|database)\b|(?<!\w)-[Ud])[\s:=,\"']{1,4}cairn(?![\w-])"),
    ("pg-credential", r"(?i:\bROLE\s+)cairn(?![\w-])"),
    ("pg-credential", r"(?<=`)cairn(?=`(?:/`postgres`)? superuser)|`cairn:cairn`|\(cairn/cairn\)"),  # prose
    ("client-storage", _NB + r"cairn_(?:data|meta|outbox)(?:_\w+)?(?![A-Za-z0-9])"),
    ("client-storage", r"(?<![\w-])cairn\.(?:db|sqlite)\b|" + _NB + r"cairn_direct\.sqlite\b|cairn-pushd\.db\b"),
    ("client-storage", r"cairn:(?:opfs-sahpool|checkpoint:|experimental:)"),
    ("wire", r"realtime:cairn:|(?<![\w-])cairn:sub:|(?<=`)cairn:(?=`)"),  # Realtime topic the SQL trigger sends on
    ("wire", r"cairn/sync/1|cairn:multitab|(?i:x-cairn-source)"),
    ("cloud-cookie", _NB + r"cairn_session(?![A-Za-z0-9])"),
    ("tauri-plugin", r"tauri[-_]plugin[-_]cairn\b|plugin:cairn\||plugins\.cairn\b"
                     r"|::new\(\"cairn\"\)|(?<![\w-])cairn:(?:default|allow-|deny-)"),
    # the deployed Supabase function (its dir too: the path rule keeps it)
    ("edge-function", r"functions/(?:v1/)?cairn-push\b|functions deploy cairn-push\b"
                      r"|functions\.supabase\.co/cairn-push\b|\"deploy\",\s*\"cairn-push\""
                      r"|(?<=deployed )cairn-push\b|(?<=pg_net → )cairn-push\b|(?<=mode's )cairn-push\b"
                      r"|(?<![\w-])cairn-push(?=(?:`|</code>)?\s+Edge\s+Function)"
                      r"|(?<=Edge Function )`?cairn-push\b"),
    ("archive", r"cairn-archive\b"),
    ("git-pin", r"unfazed-dev/cairn(?:\.git)?(?=(?:#|\?rev=|[\"']?\s*,\s*rev\s*=\s*[\"']"
                r"|\s*\n\s*ref:\s*)[0-9a-f]{7,40}\b)"),
    ("brand-metaphor", r"\b[Aa] cairn is\b|\b[Tt]he cairn (?:is\b|=)|(?<![\w-])[Cc]airns\b"),
    ("legal-entity", r"Cairn Sync, Inc\."),
    ("atlet-engine-id", r"\bEngine\.cairn(?:Direct)?\b"),  # atlet's enum, also cited in docs/
]

# Holds that apply only under some ORIGINAL paths: (class, path regex, regex).
SCOPED_HOLDS = [
    # atlet persists `Engine.name` ('cairn'/'cairnDirect': bench_runs check
    # constraint in held migration 0001, on-device db dirs) and the adapter
    # engine strings derived from it.
    ("atlet-engine-id", r"^apps/atlet/",
     r"\benum Engine \{ cairn, cairnDirect \}"
     r"|(?<![\w.$])cairn(?:Direct|-direct)?(?=['\"])|(?<=`)cairnDirect(?=`)|(?<=['\"])cairn(?:-direct)?(?=/\w+['\"])"
     r"|(?<=['\"])cairn(?=\.init['\"])|result-row-cairn(?=-)"),
    # the plugin config key under "plugins" (Tauri resolves it by plugin id)
    ("tauri-plugin", r"(?:^|/)[\w.-]*tauri\.conf\.json$", r"\"cairn\""),
    # checked-in wasm-bindgen glue: symbols baked into the committed .wasm
    ("wasm-bindgen-symbol", r"(?:^|/)(?:cairn|nostos)_ffi_wasm\.js$",
     r"__wbg_cairn\w+|(?<!\w)cairn(?:socket|engine)_\w+|\./cairn_ffi_wasm_bg\.js\b"),
]

# Specific renames, tried before the generic case map.
RENAMES = [
    (r"(?<![\w.-])(?:dev|com)\.cairn(?![\w-])", "run.nostos"),  # dev.cairn.cairn_flutter, com.cairn.sdk
    (r"(?<![\w.-])(?:dev|com)/cairn/", "run/nostos/"),           # the same, as directories
    (r"(?<!\w)@cairn(?![\w.-])", "@nostos-sync"),                # npm scope
    (r"(?<![\w-])cairn\.dev\b", "nostos.run"),
    (r"homebrew-cairn\b", "homebrew-tap"),
    (r"(?<=brew tap )unfazed-dev/cairn\b", "unfazed-dev/tap"),
    (r"unfazed-dev/cairn/cairn\b", "unfazed-dev/tap/nostos"),
]

CASES = {"cairn": "nostos", "Cairn": "Nostos", "CAIRN": "NOSTOS"}


def _compile(holds):
    alts = [f"(?P<h{i}>{rx})" for i, (_, rx) in enumerate(holds)]
    alts += [f"(?P<r{i}>{rx})" for i, (rx, _) in enumerate(RENAMES)]
    alts.append("(?P<g>cairn|Cairn|CAIRN)")
    return re.compile("|".join(alts)), [cls for cls, _ in holds]


def _profiles():
    out = {}
    for mask in range(1 << len(SCOPED_HOLDS)):
        extra = [(c, rx) for i, (c, _, rx) in enumerate(SCOPED_HOLDS) if mask >> i & 1]
        out[mask] = _compile(extra + HOLDS)
    return out


_PROFILES = _profiles()  # built once at import; read-only afterwards
_HELD_PATHS = [(cls, re.compile(rx)) for cls, rx in HELD_PATHS]
_SCOPES = [re.compile(p) for _, p, _ in SCOPED_HOLDS]
_ANY = re.compile(rb"cairn", re.I)
_RENAME_TO = [to for _, to in RENAMES]


def held_path(path):
    """Class name if `path` (original) is held verbatim, else None."""
    for cls, rx in _HELD_PATHS:
        if rx.search(path):
            return cls
    return None


def is_binary(data):
    return b"\0" in data[:8000]  # git's heuristic


def _profile(path):
    return _PROFILES[sum(1 << i for i, rx in enumerate(_SCOPES) if path and rx.search(path))]


def _in_dsn(s, i, j):
    """True if the lowercase `cairn` at s[i:j] is a postgres DSN's user,
    password or database (docker db credentials, decision 2b). Host names and
    ${CAIRN_*} expansions inside the DSN still rename."""
    ls = s.rfind("\n", 0, i) + 1
    k = max(s.rfind("postgres://", ls, i), s.rfind("postgresql://", ls, i))
    if k < 0 or any(c in s[k:i] for c in " \t\"'`<>()"):
        return False
    nxt = s[j:j + 1]
    if s[i - 3:i] == "://":
        return nxt in (":", "@")
    if s[i - 1] == ":":
        return nxt == "@"
    return s[i - 1] == "/" and "@" in s[k:i] and not (nxt.isalnum() or nxt and nxt in "_-")


def scan(path, text):
    """Yield (start, end, hold_class_or_None, replacement) for every rule match."""
    rx, classes = _profile(path)
    for m in rx.finditer(text):
        g, s = m.lastgroup, m.group()
        if g[0] == "h":
            yield m.start(), m.end(), classes[int(g[1:])], s
        elif g[0] == "r":
            yield m.start(), m.end(), None, _RENAME_TO[int(g[1:])]
        elif s == "cairn" and _in_dsn(text, m.start(), m.end()):
            yield m.start(), m.end(), "pg-dsn", s
        else:
            yield m.start(), m.end(), None, CASES[s]


def _sub(path, text):
    out, pos = [], 0
    for a, b, _, rep in scan(path, text):
        out += (text[pos:a], rep)
        pos = b
    out.append(text[pos:])
    return "".join(out)


def _apply(path, text):
    if MARKER not in text:
        return _sub(path, text)
    out, run = [], []
    for line in text.splitlines(keepends=True):
        if MARKER in line:
            out += (_sub(path, "".join(run)), line)
            run = []
        else:
            run.append(line)
    out.append(_sub(path, "".join(run)))
    return "".join(out)


_PKG_NAME = re.compile(r'^name = "([^"]+)"', re.M)
_DEPS = re.compile(r"^dependencies = \[\n(.*?)^\]", re.M | re.S)


def _sort_cargo_lock(text):
    """Cargo compares the lockfile as a string under --locked: renamed
    packages and dependency entries must move to Cargo's (byte) name order.
    Stable sorts keep the existing version/source order within a name."""
    body = text.rstrip("\n")
    chunks = body.split("\n\n")
    idx = [i for i, c in enumerate(chunks) if c.startswith("[[package]]\n")]
    pkgs = sorted((chunks[i] for i in idx), key=lambda c: _PKG_NAME.search(c).group(1))

    def deps(m):
        lines = m.group(1).splitlines(keepends=True)
        lines.sort(key=lambda ln: ln.strip().strip('",').split(" ")[0])
        return "dependencies = [\n" + "".join(lines) + "]"

    for i, c in zip(idx, pkgs):
        chunks[i] = _DEPS.sub(deps, c)
    return "\n\n".join(chunks) + text[len(body):]


def rename_path(path):
    """New repo-relative posix path. Held paths are unchanged."""
    if held_path(path) or not _ANY.search(path.encode("utf-8", "surrogateescape")):
        return path
    return _sub(None, path)


def rename_content(path, data):
    """New bytes for the blob at ORIGINAL `path`. Binaries and held paths are
    returned unchanged."""
    if held_path(path) or is_binary(data) or not _ANY.search(data):
        return data
    text = data.decode("utf-8", "surrogateescape")
    out = _apply(path, text)
    if out != text and path.rsplit("/", 1)[-1] == "Cargo.lock":
        out = _sort_cargo_lock(out)
    return out.encode("utf-8", "surrogateescape")


def rename_text(text):
    """Commit and tag messages: content rules, no path context."""
    return _apply(None, text) if "airn" in text or "AIRN" in text else text
