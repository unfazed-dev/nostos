#!/usr/bin/env python3
"""render-playbook.py — standalone HTML renderer for agent-native plan.mdx playbooks.

Turns a visual-plan `plan.mdx` into one self-contained `playbook.html` (Mermaid
via CDN; real tables, callouts, code, diagrams, columns). No Plan UI bridge, no
auth, no build step. Python stdlib only — drop into any repo and run.

Supported blocks: Mermaid, Code, Table, Callout, Checklist, QuestionForm,
FileTree, TabsBlock, AnnotatedCode, Diagram, Columns + basic markdown (headings,
bold/italic, inline code, links, lists). TabsBlock nests rich-text / annotated-
code / code blocks.

This is NOT a general MDX parser. Unknown JSX blocks fall through to prose and
render as text. Diagram bodies are author-authored HTML and are rendered
verbatim (the renderer is for local, author-controlled plan files).

Usage:
  python3 render-playbook.py [plan.mdx] [out.html] [--open]
  # plan defaults to ./plan.mdx ; out defaults to <plan-dir>/playbook.html
"""
import re, sys, html, pathlib, json, subprocess, argparse

CSS = """
:root{--fg:#1b1b1b;--muted:#5b6770;--bg:#fff;--soft:#f5f7f9;--line:#dfe4e8;--accent:#4f46e5;--warn:#b45309;--ok:#15803d}
*{box-sizing:border-box}body{font:15px/1.6 -apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif;color:var(--fg);margin:0;padding:0 0 80px}
.wrap{max-width:880px;margin:0 auto;padding:32px 24px}
h1{font-size:1.9rem;line-height:1.25;margin:0 0 6px}
h2{font-size:1.35rem;margin:34px 0 10px;padding-bottom:6px;border-bottom:1px solid var(--line)}
h3{font-size:1.08rem;margin:22px 0 8px}
h4{font-size:.98rem;margin:16px 0 6px}
code{font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;background:var(--soft);padding:1px 5px;border-radius:4px}
pre{background:#0f1115;color:#e7e9ee;padding:14px 16px;border-radius:8px;overflow:auto}
pre code{background:none;color:inherit;padding:0}
pre.mermaid{background:var(--soft);color:var(--fg);text-align:center}
.callout{border:1px solid var(--line);border-left:4px solid var(--accent);background:var(--soft);padding:12px 16px;border-radius:6px;margin:14px 0}
.callout.warn{border-left-color:var(--warn)} .callout.ok{border-left-color:var(--ok)}
.callout .h{font-weight:700;display:block;margin-bottom:4px}
table{border-collapse:collapse;width:100%;margin:14px 0;font-size:13.5px}
th,td{border:1px solid var(--line);padding:7px 10px;text-align:left;vertical-align:top}
th{background:var(--soft)} tr:nth-child(even) td{background:#fbfcfd}
ul.checklist{list-style:none;padding:0;margin:14px 0}
ul.checklist li{padding:6px 0 6px 28px;position:relative}
ul.checklist li:before{content:"☐";position:absolute;left:0;top:5px;font-size:16px;color:var(--accent)}
ul.checklist li .n{color:var(--muted);font-size:12.5px;display:block}
.qform{border:1px dashed var(--line);border-radius:8px;padding:14px 16px;margin:14px 0;background:#fbfcfd}
.qform .q{font-weight:600;margin:10px 0 4px} .qform .q:first-child{margin-top:0}
.qform .opts{margin:4px 0 8px 4px} .qform .opts label{display:block;padding:2px 0}
.qform .qsub{color:var(--muted);font-size:13px;margin:0 0 6px}
.meta{color:var(--muted);font-size:12.5px;margin:10px 0 0}
.pill{display:inline-block;font-size:11px;font-weight:600;background:var(--accent);color:#fff;padding:2px 8px;border-radius:10px;margin-right:6px}
.filetree{list-style:none;padding:0;margin:12px 0;font-size:13.5px}
.filetree li{padding:4px 0;border-bottom:1px dashed var(--line)}
.filetree li code{background:none;padding:0;color:var(--accent)}
.ft-title{font-weight:700;margin:14px 0 4px}
.ft-chg{font-size:10.5px;font-weight:700;padding:1px 6px;border-radius:8px;margin-left:6px;text-transform:uppercase;color:#fff}
.ft-added{background:var(--ok)} .ft-modified{background:var(--accent)} .ft-deleted{background:var(--warn)}
.ft-note{color:var(--muted);display:block;padding-left:8px}
.tabs{margin:14px 0}
.tab{border:1px solid var(--line);border-radius:8px;padding:4px 16px 12px;margin:10px 0;background:#fbfcfd}
.tab-h{font-size:1rem;margin:8px 0 4px;color:var(--accent);border-bottom:1px solid var(--line);padding-bottom:4px}
.tab-block{margin:6px 0}
.code-fn{font-size:12px;color:var(--muted);margin:10px 0 2px}
.ann{margin:6px 0 14px;font-size:13px;padding-left:18px}
.ann li{padding:2px 0} .ann .ann-l{font-size:11px;color:var(--muted)}
/* Diagram (author-authored HTML: boxes + arrows) */
.diagram-wrap{border:1px solid var(--line);border-radius:8px;background:var(--soft);padding:14px 16px;margin:14px 0;overflow:auto}
.diag-cap{text-align:center;color:var(--muted);font-size:13px;margin:0 0 10px}
.diagram-panel{display:flex;align-items:center;gap:12px;flex-wrap:wrap;justify-content:center}
.diagram-card{border:1px solid var(--line);border-radius:8px;padding:10px 12px;background:#fff;min-width:120px;text-align:center}
.diagram-card.diagram-accent{border-color:var(--accent);box-shadow:0 0 0 2px rgba(79,70,229,.12)}
.diagram-node{font-weight:700}
.diagram-muted{color:var(--muted);font-size:12px}
.diagram-arrow{color:var(--accent);font-size:20px;font-weight:700}
.diagram-pill{display:inline-block;font-size:11px;background:var(--soft);border:1px solid var(--line);border-radius:10px;padding:1px 8px;margin:3px 4px 0 0}
/* Columns */
.columns{display:flex;gap:16px;margin:14px 0}
.columns .col{flex:1;min-width:0}
.col-h{font-size:.95rem;margin:0 0 6px;color:var(--accent);border-bottom:1px solid var(--line);padding-bottom:3px}
@media(max-width:640px){.columns{flex-direction:column}}
"""

# block tags the renderer knows (order irrelevant; matched by alternation)
BLOCK_RE = r'<(Mermaid|Code|Table|Callout|Checklist|QuestionForm|TabsBlock|FileTree|AnnotatedCode|Diagram|Columns)\b'


def md_inline(s: str) -> str:
    s = re.sub(r'\*\*(.+?)\*\*', r'<strong>\1</strong>', s)
    s = re.sub(r'\*([^*]+)\*', r'<em>\1</em>', s)
    s = re.sub(r'`([^`]+)`', lambda m: '<code>'+html.escape(m.group(1))+'</code>', s)
    s = re.sub(r'\[([^\]]+)\]\(([^)]+)\)', r'<a href="\2">\1</a>', s)
    return s


def render_prose(text: str) -> str:
    out, para, in_list = [], [], False

    def flush():
        nonlocal para
        if para:
            out.append('<p>'+md_inline(' '.join(para))+'</p>'); para = []

    def closelist():
        nonlocal in_list
        if in_list:
            out.append('</ul>'); in_list = False

    for line in text.split('\n'):
        if not line.strip():
            flush(); closelist(); continue
        m = re.match(r'^(#{1,6})\s+(.*)', line)
        if m:
            flush(); closelist()
            out.append(f'<h{len(m.group(1))}>{md_inline(m.group(2))}</h{len(m.group(1))}>'); continue
        m = re.match(r'^[-*]\s+(.*)', line)
        if m:
            flush()
            if not in_list:
                out.append('<ul>'); in_list = True
            out.append('<li>'+md_inline(m.group(1))+'</li>'); continue
        para.append(line.strip())
    flush(); closelist()
    return '\n'.join(out)


def balanced(s, start):
    """Return the balanced {...} substring beginning at s[start] (which is '{')."""
    depth, i = 0, start
    while i < len(s):
        if s[i] == '{':
            depth += 1
        elif s[i] == '}':
            depth -= 1
            if depth == 0:
                return s[start:i+1]
        i += 1
    return s[start:]


def extract_block(src, i):
    """From index i (a '<'), return (tag, block_text, end_index) for one JSX block."""
    m = re.match(r'<(\w+)', src[i:])
    tag = m.group(1)
    self_close = src.find('/>', i)
    close_tag = src.find(f'</{tag}>', i)
    if close_tag == -1 or (self_close != -1 and self_close < close_tag):
        return tag, src[i:self_close+2], self_close+2
    return tag, src[i:close_tag+len(f'</{tag}>')], close_tag+len(f'</{tag}>')


def prop_backtick(inner, name):
    # JSX template-literal prop: name={`...`} (brace-wrapped) or name=`...`.
    # Non-greedy to the closing `} so embedded backticks in the content survive.
    m = re.search(name+r'\s*=\s*\{\s*`(.*?)`\s*\}', inner, re.S) or \
        re.search(name+r'\s*=\s*`(.*?)`\s*(?=/?>|\Z)', inner, re.S)
    return m.group(1) if m else None


def prop_dq(inner, name):
    m = re.search(name+r'\s*=\s*"([^"]*)"', inner)
    return m.group(1) if m else None


def extract_top_objects(s):
    """Yield depth-1 {...} objects from s, tolerating nested braces/arrays."""
    objs, depth, start = [], 0, None
    for i, ch in enumerate(s):
        if ch == '{':
            if depth == 0:
                start = i
            depth += 1
        elif ch == '}':
            depth -= 1
            if depth == 0 and start is not None:
                objs.append(s[start:i+1]); start = None
    return objs


def _expr_body(inner, key):
    """For a JSX prop key={<value>}, return the value with the outer { } stripped."""
    i = inner.find(key+'={')
    if i == -1:
        return None
    blob = balanced(inner, inner.find('{', i))
    return blob[1:-1] if blob else None


def json_loads_loose(blob):
    # JSX arrays/objects are JSON-legal except JS permits trailing commas; strip them.
    inner = blob.strip()[1:-1]  # strip outer { } or [ ]
    inner = re.sub(r',(\s*[}\]])', r'\1', inner)  # JS trailing commas -> JSON-legal
    return json.loads(inner)


def extract_json_string(s, key):
    """Extract a JSON-escaped string value. Matches BOTH the object-field shape
    (key: "...") and the JSX-attribute shape (key={"..."} or key="..."), unescaping
    \\n \\" \\\\ via json.loads. The JSX brace (if present) is consumed before the quote."""
    m = re.search(r'\b'+re.escape(key)+r'\s*[:=]\s*(?:\{\s*)?"', s)
    if not m:
        return None
    i, buf = m.end(), []
    while i < len(s):
        if s[i] == '\\' and i+1 < len(s):
            buf.append(s[i:i+2]); i += 2
        elif s[i] == '"':
            break
        else:
            buf.append(s[i]); i += 1
    try:
        return json.loads('"'+('').join(buf)+'"')
    except Exception:
        return ('').join(buf)


def render_mermaid(inner):
    src = prop_backtick(inner, 'source') or ''
    cap = prop_dq(inner, 'caption')
    out = f'<pre class="mermaid">{html.escape(src)}</pre>'
    if cap:
        out += f'<p style="text-align:center;color:var(--muted);font-size:13px;margin-top:4px">{html.escape(cap)}</p>'
    return out


def render_code(inner):
    code = prop_backtick(inner, 'code') or extract_json_string(inner, 'code') or ''
    fn = prop_dq(inner, 'filename') or ''
    cap = prop_dq(inner, 'caption')
    head = f'<p style="font-size:12px;color:var(--muted);margin:14px 0 2px"><code>{html.escape(fn)}</code></p>' if fn else ''
    if cap:
        head += f'<p style="font-size:12px;color:var(--muted);margin:2px 0 4px">{html.escape(cap)}</p>'
    return head + f'<pre><code>{html.escape(code)}</code></pre>'


def render_table(inner):
    ci = inner.find('columns={')
    cols, rows = [], []
    if ci != -1:
        blob = balanced(inner, inner.find('{', ci))
        try:
            cols = json_loads_loose(blob)
        except Exception:
            cols = []
    ri = inner.find('rows={')
    if ri != -1:
        blob = balanced(inner, inner.find('{', ri))
        try:
            rows = json_loads_loose(blob)
        except Exception:
            rows = []
    h = '<tr>'+''.join(f'<th>{md_inline(str(c))}</th>' for c in cols)+'</tr>'
    body = ''.join(
        '<tr>'+''.join(f'<td>{md_inline(html.unescape(str(c)))}</td>' for c in r)+'</tr>'
        for r in rows)
    return f'<table>{h}{body}</table>'


def render_callout(inner, raw):
    tone = prop_dq(inner, 'tone') or 'info'
    cls = 'warn' if 'warn' in tone else ('ok' if 'ok' in tone or 'success' in tone else '')
    m = re.search(r'>(.*)</Callout>', raw, re.S)
    body = m.group(1) if m else ''
    hm = re.match(r'\s*\*\*(.+?)\*\*\s*(.*)', body, re.S)
    if hm:
        head = f'<span class="h">{md_inline(hm.group(1))}</span>'; rest = hm.group(2)
        return f'<div class="callout {cls}">{head}{render_prose(rest)}</div>'
    return f'<div class="callout {cls}">{render_prose(body)}</div>'


def render_checklist(inner):
    out = ['<ul class="checklist">']
    body = _expr_body(inner, 'items')
    if body:
        for item in extract_top_objects(body):
            lm = re.search(r'label:\s*"([^"]*)"', item)
            nm = re.search(r'note:\s*"([^"]*)"', item)
            if lm:
                li = '<li>'+md_inline(lm.group(1))
                if nm:
                    li += f'<span class="n">{md_inline(nm.group(1))}</span>'
                out.append(li+'</li>')
    out.append('</ul>')
    return '\n'.join(out)


def render_questionform(inner):
    out = ['<div class="qform">']
    body = _expr_body(inner, 'questions')
    if body:
        for q in extract_top_objects(body):
            tm = re.search(r'title:\s*"([^"]*)"', q)
            if not tm:
                continue
            sm = re.search(r'subtitle:\s*"([^"]*)"', q)
            out.append(f'<div class="q">{md_inline(tm.group(1))}</div>')
            if sm:
                out.append(f'<div class="qsub">{md_inline(sm.group(1))}</div>')
            out.append('<div class="opts">')
            oi = q.find('options:')
            if oi != -1:
                for opt in extract_top_objects(q[oi:]):
                    lm = re.search(r'label:\s*"([^"]*)"', opt)
                    dm = re.search(r'detail:\s*"([^"]*)"', opt)
                    rec = re.search(r'recommended:\s*true', opt)
                    if lm:
                        mark = ' <i>(recommended)</i>' if rec else ''
                        det = f' — {md_inline(dm.group(1))}' if dm else ''
                        out.append(f'<label>○ {md_inline(lm.group(1))}{mark}{det}</label>')
            out.append('</div>')
    out.append('</div>')
    return '\n'.join(out)


def render_filetree(inner):
    title = prop_dq(inner, 'title') or ''
    out = []
    if title:
        out.append(f'<p class="ft-title">{md_inline(title)}</p>')
    out.append('<ul class="filetree">')
    body = _expr_body(inner, 'entries')
    if body:
        for e in extract_top_objects(body):
            pm = re.search(r'path:\s*"([^"]*)"', e)
            cm = re.search(r'change:\s*"([^"]*)"', e)
            nm = extract_json_string(e, 'note')
            if pm:
                chg = cm.group(1) if cm else ''
                badge = f'<span class="ft-chg ft-{chg}">{chg}</span>' if chg else ''
                note = f'<span class="ft-note">{md_inline(nm)}</span>' if nm else ''
                out.append(f'<li><code>{html.escape(pm.group(1))}</code> {badge}{note}</li>')
    out.append('</ul>')
    return '\n'.join(out)


def _render_annotations(blk):
    """Shared annotation-list renderer (AnnotatedCode top-level + inside TabsBlock).
    Matches the annotations array in either shape — object field (annotations: [ … ])
    or JSX attribute (annotations={[ … ]}) — then scans to the matching ] and extracts
    the {...} annotation objects INSIDE the array (skipping the JSX wrapper brace)."""
    items = []
    m = re.search(r'\bannotations\s*[:=]\s*\{?\s*\[', blk)
    if not m:
        return ''
    bi = blk.find('[', m.start())
    # scan to the matching ], ignoring brackets inside "..." strings — annotation
    # notes routinely contain ["…"], [T], array refs that would otherwise close the
    # array early. Backslash escapes inside the string are honoured.
    j, depth, in_str = bi, 0, False
    while j < len(blk):
        c = blk[j]
        if in_str:
            if c == '\\':
                j += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == '[':
            depth += 1
        elif c == ']':
            depth -= 1
            if depth == 0:
                break
        j += 1
    for a in extract_top_objects(blk[bi+1:j]):
        lm = re.search(r'label:\s*"([^"]*)"', a)
        ln = re.search(r'lines:\s*"([^"]*)"', a)
        note = extract_json_string(a, 'note')
        if lm:
            items.append(f'<li><b>{md_inline(lm.group(1))}</b>'
                         + (f' <span class="ann-l">L{ln.group(1)}</span>' if ln else '')
                         + (f': {md_inline(note)}' if note else '') + '</li>')
    return '<ul class="ann">'+''.join(items)+'</ul>' if items else ''


def _render_annotated_code(blk):
    """filename + code + annotations — used top-level and inside tabs."""
    fn = extract_json_string(blk, 'filename') or prop_dq(blk, 'filename') or ''
    code = extract_json_string(blk, 'code') or ''
    head = f'<p class="code-fn"><code>{md_inline(fn)}</code></p>' if fn else ''
    return f'{head}<pre><code>{html.escape(code)}</code></pre>{_render_annotations(blk)}'


def render_inner_block(blk):
    """Render one tab block: annotated-code/code → code renderer; anything else (rich-text) → markdown prose."""
    tm = re.search(r'type:\s*"([^"]*)"', blk)
    if tm and tm.group(1) in ('annotated-code', 'code'):
        return _render_annotated_code(blk)
    md = extract_json_string(blk, 'markdown')
    return f'<div class="tab-block">{render_prose(md or "")}</div>' if md else ''


def render_tabsblock(inner):
    out = ['<div class="tabs">']
    body = _expr_body(inner, 'tabs')
    if body:
        for tab in extract_top_objects(body):
            lm = re.search(r'label:\s*"([^"]*)"', tab)
            if not lm:
                continue
            out.append(f'<div class="tab"><h3 class="tab-h">{md_inline(lm.group(1))}</h3>')
            bi = tab.find('blocks:')
            if bi != -1:
                for blk in extract_top_objects(tab[bi:]):
                    r = render_inner_block(blk)
                    if r:
                        out.append(r)
            out.append('</div>')
    out.append('</div>')
    return '\n'.join(out)


def render_diagram(inner):
    """caption + author-authored HTML (a ```html fence inside the block), rendered verbatim."""
    cap = prop_dq(inner, 'caption')
    m = re.search(r'```(?:html)?\n(.*?)```', inner, re.S)
    body = m.group(1) if m else ''
    out = ['<div class="diagram-wrap">']
    if cap:
        out.append(f'<p class="diag-cap">{md_inline(cap)}</p>')
    if body:
        out.append(body)
    else:
        out.append('<p class="diag-cap">(empty diagram)</p>')
    out.append('</div>')
    return '\n'.join(out)


def render_columns(inner):
    """<Columns> with <Column label="…">markdown</Column> children → responsive flex row."""
    out = ['<div class="columns">']
    for cm in re.finditer(r'<Column\b([^>]*)>(.*?)</Column>', inner, re.S):
        attrs, body = cm.group(1), cm.group(2)
        lm = re.search(r'label\s*=\s*"([^"]*)"', attrs)
        out.append('<div class="col">')
        if lm:
            out.append(f'<h4 class="col-h">{md_inline(lm.group(1))}</h4>')
        out.append(render_prose(body.strip()))
        out.append('</div>')
    out.append('</div>')
    return '\n'.join(out)


RENDERERS = {
    'Mermaid': render_mermaid, 'Code': render_code, 'Table': render_table,
    'Checklist': render_checklist, 'QuestionForm': render_questionform,
    'FileTree': render_filetree, 'TabsBlock': render_tabsblock,
    'AnnotatedCode': _render_annotated_code, 'Diagram': render_diagram,
    'Columns': render_columns,
}


def convert(mdx: str) -> str:
    out, i, prose = [], 0, []

    def flush_prose():
        nonlocal prose
        if prose:
            out.append(render_prose(''.join(prose))); prose = []

    while i < len(mdx):
        if mdx[i] == '<' and re.match(BLOCK_RE, mdx[i:]):
            flush_prose()
            tag, inner, end = extract_block(mdx, i)
            if tag == 'Callout':
                out.append(render_callout(inner, mdx[i:end]))
            elif tag in RENDERERS:
                out.append(RENDERERS[tag](inner))
            i = end
        else:
            prose.append(mdx[i]); i += 1
    flush_prose()
    body = '\n'.join(out)
    body = re.sub(r'(<h1>.*?</h1>)',
                  r'\1<p class="meta"><span class="pill">playbook</span> rendered from '
                  r'<code>plan.mdx</code> · regenerate with <code>render-playbook.py</code></p>',
                  body, count=1)
    return f'''<!doctype html><html lang="en"><head><meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<title>Playbook</title>
<script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
<script>mermaid.initialize({{startOnLoad:true,securityLevel:'loose',theme:'neutral'}});</script>
<style>{CSS}</style></head>
<body><div class="wrap">
{body}
</div></body></html>'''


def maybe_open(path):
    opener = {'darwin': 'open', 'linux': 'xdg-open', 'win32': 'start'}.get(sys.platform)
    if not opener:
        return
    cmd = ['cmd', '/c', 'start', '', str(path)] if opener == 'start' else [opener, str(path)]
    try:
        subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        pass


def main():
    p = argparse.ArgumentParser(
        prog='render-playbook.py',
        description='Render an agent-native plan.mdx playbook to a standalone HTML file.')
    p.add_argument('plan', nargs='?', default='plan.mdx', help='path to plan.mdx (default: ./plan.mdx)')
    p.add_argument('out', nargs='?', default=None, help='output html (default: <plan-dir>/playbook.html)')
    p.add_argument('--open', action='store_true', help='open the rendered html in the default browser')
    args = p.parse_args()

    src = pathlib.Path(args.plan)
    if not src.is_file():
        sys.exit(f'render-playbook: plan not found: {src}')
    dst = pathlib.Path(args.out) if args.out else src.parent / 'playbook.html'
    mdx = src.read_text()
    html_out = convert(mdx)
    # derive <title> from the first H1 when present
    t = re.search(r'^#\s+(.+)$', mdx, re.M)
    if t:
        html_out = html_out.replace('<title>Playbook</title>', f'<title>{html.escape(t.group(1))}</title>')
    dst.write_text(html_out)
    print(f'wrote {dst}')
    if args.open:
        maybe_open(dst)


if __name__ == '__main__':
    main()
