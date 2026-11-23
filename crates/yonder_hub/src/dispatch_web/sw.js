// Shows the hub's notifications and opens the thread one is about when it's
// tapped. iPhones only deliver these to the Home Screen app.
'use strict';

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('push', (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    data = { body: event.data ? event.data.text() : '' };
  }
  const shown = self.registration.showNotification(data.title || 'Dispatch', {
    body: data.body || '',
    tag: data.tag || undefined,
    icon: '/dispatch/icon-192.png',
    badge: '/dispatch/icon-192.png',
    data: { url: data.url || '/dispatch/' },
  });
  event.waitUntil(Promise.all([shown, updateBadge(data.waiting)]));
});

// The app icon counts threads waiting for an approval.
async function updateBadge(waiting) {
  if (typeof waiting !== 'number' || !self.navigator.setAppBadge) return;
  try {
    if (waiting > 0) await self.navigator.setAppBadge(waiting);
    else await self.navigator.clearAppBadge();
  } catch {
    // Badges are a nicety; a refused one changes nothing else.
  }
}

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const target = new URL(event.notification.data?.url || '/dispatch/', self.location.origin);
  if (target.origin !== self.location.origin || !target.pathname.startsWith('/dispatch')) return;
  event.waitUntil(openThread(target.href));
});

async function openThread(url) {
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const open = windows.find((client) => new URL(client.url).pathname.startsWith('/dispatch'));
  if (open) {
    await open.focus();
    open.postMessage({ type: 'open', url });
    return;
  }
  await self.clients.openWindow(url);
}
