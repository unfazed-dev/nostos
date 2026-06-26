/**
 * Theme store — the single source of truth for Nostos's resolved theme.
 *
 * Three modes: 'system' (follows prefers-color-scheme, the default), 'light',
 * 'dark'. The chosen mode is mirrored onto <html data-theme> (which the token
 * blocks in tokens.css resolve) and persisted to localStorage, where app.html's
 * no-flash bootstrap reads it on the next load.
 */
import { browser } from '$app/environment';

export type ThemeMode = 'system' | 'light' | 'dark';

const STORAGE_KEY = 'nostos-theme';

function initial(): ThemeMode {
  if (!browser) return 'system';
  const saved = localStorage.getItem(STORAGE_KEY);
  return saved === 'light' || saved === 'dark' || saved === 'system' ? saved : 'system';
}

let mode = $state<ThemeMode>(initial());

function apply(next: ThemeMode) {
  if (!browser) return;
  document.documentElement.setAttribute('data-theme', next);
  try { localStorage.setItem(STORAGE_KEY, next); } catch { /* ignore */ }
}

// apply on first client render so the html attr matches the stored mode
if (browser) apply(mode);

export const theme = {
  get mode() { return mode; },
  set(next: ThemeMode) { mode = next; apply(next); }
};
