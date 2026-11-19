'use strict';
/**
 * Мост для Tauri: тот же `window.setup`, что давал preload Electron-установщика,
 * поэтому app.js менять не пришлось. В Electron этот файл не подключается.
 */
(() => {
  const tauri = window.__TAURI__;
  if (!tauri || !tauri.core || window.setup) return;
  const invoke = tauri.core.invoke;

  window.setup = {
    info: () => invoke('setup_info'),
    chooseDir: (current) => invoke('setup_choose_dir', { current: current ?? null }),
    checkDir: (dir) => invoke('setup_check_dir', { dir }),
    install: (opts) => invoke('setup_install', { opts }),
    cancel: () => invoke('setup_cancel'),
    launch: () => invoke('setup_launch'),
    minimize: () => invoke('setup_minimize'),
    close: () => invoke('setup_close'),
    onProgress: (cb) => {
      let unlisten = null;
      let closed = false;
      tauri.event.listen('setup:progress', (event) => cb(event.payload)).then((fn) => {
        unlisten = fn;
        if (closed) fn();
      });
      return () => {
        closed = true;
        if (unlisten) unlisten();
      };
    },
  };
})();
