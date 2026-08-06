<script lang="ts">
	import { onMount } from 'svelte';
	import { Button } from '$lib/components/ui';

	type RulesTable = { table: string; scope: string };
	type RulesResponse = {
		sync_mode: string;
		checksum: string;
		sync_epoch: string;
		tables: RulesTable[];
	};
	type EditableRow = { table: string; sync: boolean; scope: string };

	const STORAGE_KEY = 'nostos.adminServerUrl';

	let serverUrl = $state('http://localhost:8800');
	let token = $state(''); // never persisted — memory only, for this session
	let loading = $state(false);
	let saving = $state(false);
	let loadError = $state<string | null>(null);
	let saveError = $state<string | null>(null);
	let data = $state<RulesResponse | null>(null);
	let rows = $state<EditableRow[]>([]);

	let readOnly = $derived(data?.sync_mode === 'hand');
	let isAllMode = $derived(data?.sync_mode === 'all');
	let isToggles = $derived(data?.sync_mode === 'toggles');

	onMount(() => {
		try {
			const saved = localStorage.getItem(STORAGE_KEY);
			if (saved) serverUrl = saved;
		} catch {
			/* ignore — private-browsing / storage disabled */
		}
		if (serverUrl.trim()) load();
	});

	function persistUrl() {
		try {
			localStorage.setItem(STORAGE_KEY, serverUrl);
		} catch {
			/* ignore */
		}
	}

	function normalizedUrl(): string {
		return serverUrl.trim().replace(/\/+$/, '');
	}

	function applyResponse(body: RulesResponse) {
		data = body;
		rows = body.tables.map((t) => ({ table: t.table, sync: true, scope: t.scope }));
	}

	async function errorFrom(res: Response): Promise<string> {
		const body = await res.json().catch(() => null);
		return (body && typeof body.error === 'string' && body.error) || `${res.status} ${res.statusText}`;
	}

	async function load() {
		loading = true;
		loadError = null;
		saveError = null;
		try {
			const res = await fetch(`${normalizedUrl()}/rules`);
			if (!res.ok) throw new Error(await errorFrom(res));
			applyResponse((await res.json()) as RulesResponse);
		} catch (e) {
			loadError = e instanceof Error ? e.message : String(e);
			data = null;
			rows = [];
		} finally {
			loading = false;
		}
	}

	async function save() {
		// ponytail: guarded at call sites too (button hidden outside toggles mode) —
		// this is the load-bearing check, since sync_mode="all" always GETs tables:[]
		// and PUT is a full replace, so sending it would silently wipe the CLI's toggle set.
		if (!data || data.sync_mode !== 'toggles') return;
		saving = true;
		saveError = null;
		const payload = {
			sync_mode: data.sync_mode,
			tables: rows.map((r) => ({
				table: r.table,
				sync: r.sync,
				scope: r.scope.trim() === '' ? null : r.scope.trim()
			}))
		};
		try {
			const res = await fetch(`${normalizedUrl()}/rules`, {
				method: 'PUT',
				headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
				body: JSON.stringify(payload)
			});
			if (!res.ok) throw new Error(await errorFrom(res));
			applyResponse((await res.json()) as RulesResponse);
		} catch (e) {
			saveError = e instanceof Error ? e.message : String(e);
		} finally {
			saving = false;
		}
	}
</script>

<svelte:head>
	<title>Sync rules — Nostos admin</title>
</svelte:head>

<section class="wrap">
	<div class="admin-head">
		<a class="back" href="/admin">&larr; Admin</a>
		<h1>Sync rules</h1>
	</div>

	<div class="panel">
		<label class="field">
			<span>Server URL</span>
			<input
				type="text"
				bind:value={serverUrl}
				oninput={persistUrl}
				placeholder="http://localhost:8800"
			/>
		</label>
		<label class="field">
			<span>Admin token</span>
			<input type="password" bind:value={token} autocomplete="off" placeholder="NOSTOS_ADMIN_TOKEN" />
		</label>
		<p class="hint">
			The token is kept in memory for this session only — never stored. Reloading this page clears
			it and you'll need to re-enter it.
		</p>
		<Button type="button" variant="solid" size="sm" onclick={load} disabled={loading || !serverUrl.trim()}>
			{loading ? 'Loading…' : 'Load'}
		</Button>
		{#if loadError}
			<p class="err">{loadError}</p>
		{/if}
	</div>

	{#if data}
		{#if isAllMode}
			<div class="banner">
				<p>WARNING: sync_mode = "all" — every replicated row reaches every authorised client.</p>
				<p>
					This is the zero-config development default. For production, run
					<code>nostos rules init</code> and switch sync_mode to "toggles".
				</p>
			</div>
		{/if}
		{#if readOnly}
			<div class="banner">
				<p>
					sync_mode = "hand" — this ruleset is hand-authored. <code>PUT /rules</code> rejects hand-mode
					writes, so this panel is read-only. Edit with <code>nostos rules edit --mode hand</code>.
				</p>
			</div>
		{/if}

		<div class="panel">
			<dl class="meta">
				<dt>sync_mode</dt>
				<dd class="mono">{data.sync_mode}</dd>
				<dt>checksum</dt>
				<dd class="mono">{data.checksum}</dd>
				<dt>sync_epoch</dt>
				<dd class="mono">{data.sync_epoch}</dd>
			</dl>
		</div>

		<div class="panel">
			{#if rows.length === 0}
				<p class="empty">
					{isAllMode
						? 'No per-table entries apply in "all" mode.'
						: 'No tables are currently reported as synced. Toggled-off or unlisted tables never appear here — add or re-enable them with `nostos rules edit`.'}
				</p>
			{:else}
				<table>
					<thead>
						<tr>
							<th>Sync</th>
							<th>Table</th>
							<th>Scope</th>
						</tr>
					</thead>
					<tbody>
						{#each rows as row (row.table)}
							<tr>
								<td><input type="checkbox" bind:checked={row.sync} disabled={readOnly} /></td>
								<td class="mono">{row.table}</td>
								<td>
									<input
										type="text"
										bind:value={row.scope}
										disabled={readOnly}
										placeholder="whole table"
									/>
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			{/if}
		</div>

		{#if isToggles}
			<div class="panel">
				<Button type="button" variant="mark" size="sm" onclick={save} disabled={saving || !token.trim()}>
					{saving ? 'Saving…' : 'Save'}
				</Button>
				<p class="hint">
					Save is a full replace, not a patch, and there's no optimistic concurrency: it writes
					exactly what's shown above, silently overwriting a concurrent CLI edit and dropping any
					table this panel doesn't know about (already off, or added since you last loaded) from the
					file entirely. Load right before you Save; use <code>nostos rules edit</code> for tables
					that never appear here.
				</p>
				{#if saveError}
					<p class="err">{saveError}</p>
				{/if}
			</div>
		{/if}
	{/if}
</section>

<style>
	.wrap {
		max-width: 720px;
		margin: 0 auto;
		padding: 44px 20px 80px;
		display: flex;
		flex-direction: column;
		gap: 20px;
	}
	.admin-head {
		display: flex;
		align-items: baseline;
		gap: 16px;
	}
	.back {
		color: var(--ink-soft);
		text-decoration: none;
		font-size: var(--t-14);
	}
	.back:hover {
		color: var(--mark);
	}
	h1 {
		font-size: var(--t-28);
		margin: 0;
	}
	.panel {
		padding: 28px 26px;
		border: 1px solid var(--rule);
		border-radius: var(--radius-lg);
		background: var(--paper-2);
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 6px;
		font-size: var(--t-14);
	}
	.field span {
		color: var(--ink-soft);
	}
	input[type='text'],
	input[type='password'] {
		font: inherit;
		padding: 8px 10px;
		border: 1px solid var(--stone-2);
		border-radius: var(--radius);
		background: var(--paper);
		color: var(--ink);
	}
	input[type='text']:focus,
	input[type='password']:focus {
		outline: 2px solid var(--mark);
		outline-offset: 1px;
	}
	.hint {
		font-size: var(--t-12);
		color: var(--ink-soft);
		margin: 0;
	}
	.err {
		font-size: var(--t-14);
		color: #c0392b;
		white-space: pre-wrap;
		margin: 0;
	}
	.banner {
		padding: 16px 20px;
		border: 1px solid var(--mark);
		border-radius: var(--radius-lg);
		background: var(--mark-glow);
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.banner p {
		margin: 0;
		font-size: var(--t-14);
	}
	.meta {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 4px 12px;
		margin: 0;
		font-size: var(--t-14);
	}
	.meta dt {
		color: var(--ink-soft);
	}
	.mono {
		font-family: var(--font-mono);
		word-break: break-all;
	}
	.empty {
		color: var(--ink-soft);
		font-size: var(--t-14);
		margin: 0;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--t-14);
	}
	th,
	td {
		text-align: left;
		padding: 8px 10px;
		border-bottom: 1px solid var(--rule);
	}
	td input[type='text'] {
		width: 100%;
	}
</style>
