// atlet push pilot — service worker for the raw Web Push rail (ADR-0037 §1).
// No Firebase on the web: the browser's own push service delivers, cairn's
// WebPushRail (crates/nostos-infra/src/push/webpush.rs) signs with VAPID.
//
// Payload contract (webpush.rs): visible pushes are {title, body}; silent
// doorbells are {table, lsn} with no user-facing copy.
/* eslint-disable no-restricted-globals */
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('push', (event) => {
  console.log('[atlet-sw] push event', event.data && event.data.text());
  let data = {};
  try {
    data = event.data.json();
  } catch (_) {
    // No/undecodable payload: still surface something — a data-less push is
    // otherwise invisible and indistinguishable from a lost one (the
    // synthetic DevTools "push" button sends exactly this shape).
    data = {};
  }
  // Visible pushes carry {title, body}. Silent doorbells ({table, lsn}) and
  // payload-less pushes get a neutral fallback so delivery is always
  // observable.
  const title = data.title || 'Atlet';
  const body = data.body || 'Background sync';
  event.waitUntil(
    self.registration.showNotification(title, {
      body,
      tag: 'cairn',
      renotify: true,
      icon: '/icons/Icon-192.png',
      badge: '/icons/Icon-192.png',
      data: { url: '/' },
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(
    (async () => {
      const all = await self.clients.matchAll({
        type: 'window',
        includeUncontrolled: true,
      });
      for (const client of all) {
        if ('focus' in client) return client.focus();
      }
      return self.clients.openWindow('/');
    })(),
  );
});
