/**
 * Where the client thinks the server is, and what it was handed to talk to it.
 *
 * Same-origin by default: the dev server proxies `/v1`, and a deployed client is
 * served from somewhere that does the same. A token is only needed when the
 * client is loaded from an origin the daemon does not proxy for — which is not
 * the normal case, so it is read from storage rather than being a build-time
 * constant nobody can change without a rebuild.
 */
export const ARGUS_BASE = import.meta.env.VITE_ARGUS_URL ?? "";

/** Device token, if this client has been paired. */
export function deviceToken(): string | null {
  try {
    return localStorage.getItem("argus.token");
  } catch {
    // Private browsing, or storage disabled. Not fatal: same-origin loopback
    // needs no token at all.
    return null;
  }
}

export function setDeviceToken(token: string): void {
  try {
    localStorage.setItem("argus.token", token);
  } catch {
    /* nothing sensible to do; the session simply will not persist */
  }
}
