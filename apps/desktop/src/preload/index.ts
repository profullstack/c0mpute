// The renderer's only door to the main process: invoke a named handler, or
// listen for a named event. Channel names are checked against fixed prefixes.

import { contextBridge, ipcRenderer } from "electron";

const INVOKE = /^(app|settings|chat|cli|node|hosting|connect|account|tasks|agent|mcp|skills):[A-Za-z]+$/;
const EVENTS = new Set(["agent:event", "agent:confirm", "hosting:update", "connect:update", "mcp:update", "tasks:update", "account:login"]);

contextBridge.exposeInMainWorld("c0", {
  invoke: async (channel: string, ...args: unknown[]) => {
    if (!INVOKE.test(channel)) throw new Error(`blocked channel ${channel}`);
    const r = (await ipcRenderer.invoke(channel, ...args)) as { ok: boolean; value?: unknown; error?: string };
    if (!r.ok) throw new Error(r.error);
    return r.value;
  },
  on: (event: string, cb: (payload: unknown) => void) => {
    if (!EVENTS.has(event)) throw new Error(`blocked event ${event}`);
    const fn = (_: unknown, p: unknown) => cb(p);
    ipcRenderer.on(event, fn);
    return () => ipcRenderer.removeListener(event, fn);
  },
});
