import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/**
 * Tauri host: the Rust backend (`src-tauri`) replaces the Electron main process.
 * The rest of the renderer keeps using `window.chald.invoke/on`, so no page has to know the difference.
 * In Electron this file does nothing.
 */
const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

if (isTauri && !window.chald) {
  const listeners = new Map<string, Set<(payload: unknown) => void>>();
  let eventsStarted: Promise<void> | null = null;

  const ensureEvents = (): void => {
    if (eventsStarted) return;
    eventsStarted = listen<{ channel: string; payload: unknown }>('chald:event', (event) => {
      listeners.get(event.payload.channel)?.forEach((cb) => cb(event.payload.payload));
    })
      .then(() => undefined)
      .catch(() => undefined);
  };

  window.chald = {
    setZoom: () => undefined,
    invoke: (channel: string, arg?: unknown) => invoke('chald_dispatch', { channel, arg: arg ?? null }),
    on: (channel: string, cb: (payload: unknown) => void) => {
      ensureEvents();
      if (!listeners.has(channel)) listeners.set(channel, new Set());
      listeners.get(channel)!.add(cb);
      return () => {
        listeners.get(channel)?.delete(cb);
      };
    },
  };
}

export {};
