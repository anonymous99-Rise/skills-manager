/**
 * Transport layer: routes every backend call through Tauri IPC when running
 * as the desktop app, or through HTTP against the skills-manager-server when
 * running as a plain web page (Cloudflare Pages / self-hosted server).
 *
 * The command names, argument shapes and return values are identical on both
 * transports — the server exposes POST /api/invoke/:cmd and the handlers
 * return the same DTOs the Tauri commands do.
 */

export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

class WebInvokeError extends Error {
  status: number;

  constructor(message: string, status: number) {
    super(message);
    this.status = status;
  }
}

export async function invoke<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<T>(cmd, args);
  }

  const res = await fetch(`/api/invoke/${encodeURIComponent(cmd)}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args ?? {}),
  });
  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText);
    if (res.status === 401) {
      // Not signed in — bounce to the server's login page (OAuth + PAT).
      const login = "/api/auth/login";
      if (!window.location.pathname.startsWith("/api/auth")) {
        window.location.href = login;
      }
    }
    throw new WebInvokeError(text || `request failed (${res.status})`, res.status);
  }
  const text = await res.text();
  return (text ? JSON.parse(text) : null) as T;
}

type UnlistenFn = () => void;

export async function listen<T = unknown>(
  _event: string,
  _handler: (event: { payload: T }) => void,
): Promise<UnlistenFn> {
  if (isTauri) {
    const { listen } = await import("@tauri-apps/api/event");
    return listen<T>(_event, _handler);
  }
  // Web mode: the server pushes no events. Callers treat this as optional UX.
  return () => {};
}

/** Open a URL outside the app. */
export async function openUrl(url: string): Promise<void> {
  if (isTauri) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
    return;
  }
  window.open(url, "_blank", "noopener");
}

/** Native folder/file picker; web mode has no equivalent. */
export async function openDialog(
  options?: Record<string, unknown>,
): Promise<string | string[] | null> {
  if (isTauri) {
    const { open } = await import("@tauri-apps/plugin-dialog");
    return open(options as never) as Promise<string | string[] | null>;
  }
  throw new Error("The folder picker is desktop-only. Paste a Git URL instead.");
}

export async function dialogConfirm(
  message: string,
  options?: Record<string, unknown>,
): Promise<boolean> {
  if (isTauri) {
    const { confirm } = await import("@tauri-apps/plugin-dialog");
    return confirm(message, options as never);
  }
  return window.confirm(message);
}

export async function writeText(text: string): Promise<void> {
  if (isTauri) {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
    return;
  }
  await navigator.clipboard.writeText(text);
}

/** Desktop updater check; web deployments update via their own channel. */
export async function checkAppUpdater(
  options?: Record<string, unknown>,
): Promise<{
  downloadAndInstall: (options?: Record<string, unknown>) => Promise<void>;
} | null> {
  if (isTauri) {
    const { check } = await import("@tauri-apps/plugin-updater");
    const update = await check(options as never);
    if (!update) return null;
    return {
      downloadAndInstall: (opts?: Record<string, unknown>) =>
        update.downloadAndInstall(undefined, opts as never),
    };
  }
  throw new Error("The in-app updater is desktop-only.");
}

export function startWindowDrag(): void {
  if (!isTauri) return;
  import("@tauri-apps/api/window")
    .then(({ getCurrentWindow }) => getCurrentWindow().startDragging())
    .catch(() => {});
}
