import React from 'react';
import type { Host } from '..';
import {
  appBridge,
  ask,
  bridgeStore,
  type ShellAnswer,
  type VideoOutput,
} from './bridge';
import { settings, type TvWidth } from '@/lib/settings';
import { shellDownloads } from './downloads';
import { appEngine } from './player';
import { resolveYouTube } from './youtube';

export type DiscordStatus = ShellAnswer<'discord-status'>;

export const useDiscordStatus = bridgeStore('discord-status').use;

export function checkDiscord(): void {
  appBridge()?.send({ type: 'discord-check' });
}

/** The browser's own menu only where it edits or copies; Shift still opens it. */
function onContextMenu(e: MouseEvent) {
  const target = e.target as HTMLElement | null;
  const editable = target?.closest('input, textarea, [contenteditable="true"]');
  if (e.shiftKey || editable || !!window.getSelection()?.toString()) return;
  e.preventDefault();
}

let windowFullscreen = false;

/** The way the window is going, from a request until the page takes the new size. */
let expected: { into: boolean; until: number } | null = null;

function sendFullscreen(value?: boolean): void {
  const shell = appBridge();
  if (!shell) return;
  expected = {
    into: value ?? !windowFullscreen,
    until: performance.now() + 500,
  };
  shell.send(
    value === undefined ? { type: 'fullscreen' } : { type: 'fullscreen', value }
  );
}

export function leaveWindowFullscreen(): void {
  if (windowFullscreen) sendFullscreen(false);
}

/** The desktop app's window in and out of full screen, on any screen; false elsewhere. */
export function toggleWindowFullscreen(): boolean {
  const shell = appBridge();
  if (!shell || shell.platform === 'android') return false;
  sendFullscreen();
  return true;
}

export function exitWindowFullscreen(): void {
  sendFullscreen(false);
}

// The window's own corner, which stays put as it changes size.
// eslint-disable-next-line lingui/no-unlocalized-strings -- a CSS value
const PINNED = 'top left';

// A strong ease-out: the edge is nearly there at once and settles gently.
const SETTLE_EASE = 'cubic-bezier(0.23, 1, 0.32, 1)';

/** The page's size before the last resize, which the window's edges moved from. */
let pageSize = { width: window.innerWidth, height: window.innerHeight };

/** How far an edge may appear to travel, so a small window doesn't squash the page. */
const clampStretch = (ratio: number) => Math.min(1.05, Math.max(0.95, ratio));

/**
 * The page follows the window's edges instead of jumping to the new size: it
 * starts at the size it had, pinned to the window's top left like the window
 * itself, and stretches out with the bottom edge into full screen or draws back
 * with it. It starts in the resize, before the browser draws the new size, so
 * no frame shows the jump. The player is left alone, as the video beneath it
 * doesn't move with the page, and reduced motion gets the plain switch.
 */
function followEdges(
  into: boolean,
  from: { width: number; height: number }
): void {
  const root = document.getElementById('root');
  if (
    !root ||
    document.documentElement.classList.contains('playing') ||
    matchMedia('(prefers-reduced-motion: reduce)').matches
  )
    return;
  const x = clampStretch(from.width / window.innerWidth);
  const y = clampStretch(from.height / window.innerHeight);
  // A press during the last one goes on from where it had got to.
  const running = root.getAnimations().length > 0;
  const transform = running
    ? getComputedStyle(root).transform
    : `scale(${x}, ${y})`;
  root.getAnimations().forEach((a) => a.cancel());
  if (transform === 'none' || (x === 1 && y === 1 && !running)) return;
  root.animate(
    [
      { transform, transformOrigin: PINNED },
      { transform: 'none', transformOrigin: PINNED },
    ],
    // The edge travels a few dozen pixels; out is quicker, as exits are.
    { duration: into ? 220 : 180, easing: SETTLE_EASE }
  );
}

function onResize(): void {
  const from = pageSize;
  pageSize = { width: window.innerWidth, height: window.innerHeight };
  const next = expected;
  expected = null;
  if (next && performance.now() < next.until) followEdges(next.into, from);
}

function sendLayout(): void {
  appBridge()?.send({
    type: 'tv-layout',
    value: settings.app.tvLayout.read(),
    width: settings.app.tvWidth.read(),
  });
}

/**
 * Lays the TV layout out this wide: the window zooms to it, and the page
 * follows the new size as it does any resize.
 */
export function setTvWidth(width: TvWidth): void {
  settings.app.tvWidth.write(width);
  sendLayout();
}

/**
 * Switches the TV layout and lays the page out again in it: the window zooms
 * to the TV's page width, and the page reloads, as much of it reads the layout
 * once as it draws.
 */
export function setTvLayout(on: boolean): void {
  settings.app.tvLayout.write(on);
  sendLayout();
  window.location.reload();
}

/** Follows the window's full screen and the chosen layout, and keeps the browser's menu off the page. */
export function setupDesktop(): () => void {
  // The app keeps its own note of the layout, which this brings in line with
  // the settings: after a switch the bridge's `tv` is stale, so it can't tell
  // whether they differ, and sending the same layout again changes nothing.
  sendLayout();
  const unsubscribe = appBridge()?.subscribe((m) => {
    if (m.type === 'fullscreen') windowFullscreen = m.value;
  });
  window.addEventListener('contextmenu', onContextMenu);
  window.addEventListener('resize', onResize);
  return () => {
    unsubscribe?.();
    window.removeEventListener('contextmenu', onContextMenu);
    window.removeEventListener('resize', onResize);
  };
}

export type AudioDevice = { name: string; description: string };

/** The outputs mpv can play to, asked for each time they are shown. */
export function useAudioDevices(): AudioDevice[] | null {
  const [devices, setDevices] = React.useState<AudioDevice[] | null>(null);
  React.useEffect(() => {
    let live = true;
    ask({ type: 'audio-devices' }, 'audio-devices')
      .then((m) => live && setDevices(m.devices ?? []))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  return devices;
}

export function openMpvConfig(): void {
  appBridge()?.send({ type: 'open-mpv-config' });
}

export type VideoOutputState = ShellAnswer<'video-output'>;

/** The outputs the app offers, and a choice that applies from its next start. */
export function useVideoOutput(): [
  VideoOutputState | null,
  (output: VideoOutput) => void,
] {
  const [state, setState] = React.useState<VideoOutputState | null>(null);
  const request = React.useCallback((output: VideoOutput | null) => {
    ask({ type: 'video-output', output }, 'video-output', { timeoutMs: 5000 })
      .then(setState)
      .catch(() => undefined);
  }, []);
  React.useEffect(() => request(null), [request]);
  return [state, request];
}

export function restartApp(): void {
  appBridge()?.send({ type: 'restart' });
}

export const desktopHost: Host = {
  name: 'desktop',
  get tv() {
    return settings.app.tvLayout.read();
  },
  device: () => ({ name: appBridge()?.device }),
  player: appEngine,
  playerFeatures: [
    'audio',
    'chapters',
    'stats',
    'headers',
    'upscaling',
    'anyFormat',
  ],
  resolveYouTube,
  downloads: {
    ...shellDownloads,
    folder: {
      choose: () => appBridge()?.send({ type: 'download-folder' }),
      open: (id) =>
        appBridge()?.send({ type: 'download-open', id: id ?? null }),
    },
  },
  back: () => {
    if (!windowFullscreen) return false;
    sendFullscreen(false);
    return true;
  },
};
