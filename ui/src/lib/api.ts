// Same-origin /api/* client. Cross-origin pages are rejected with 403
// and network failures get the same hint, since both mean the caller is
// not this dashboard.

const SAME_ORIGIN_HINT = "the dashboard only accepts same-origin requests";

// Error bodies arrive either as the OpenAI envelope
// {"error":{message,type,..}} or a bare {"error":".."}; flatten both to
// one display string.
export function apiError(data: unknown, status: number): string {
  const err = (data as { error?: unknown } | null)?.error;
  let msg: string;
  if (err && typeof err === "object") {
    const e = err as { message?: string; type?: string };
    msg = e.message || e.type || JSON.stringify(err);
  } else if (typeof err === "string") {
    msg = err;
  } else {
    msg = `HTTP ${status}`;
  }
  if (status === 403) msg += ` - ${SAME_ORIGIN_HINT}`;
  return msg;
}

export async function api<T = Record<string, unknown>>(
  path: string,
  opts?: RequestInit,
): Promise<T> {
  let r: Response;
  try {
    r = await fetch(path, opts);
  } catch (e) {
    throw new Error(
      `Request failed - ${SAME_ORIGIN_HINT} (${String((e as Error).message || e)})`,
    );
  }
  let data: unknown = {};
  try {
    data = await r.json();
  } catch {
    /* empty or non-JSON body */
  }
  if (!r.ok || (data && (data as { error?: unknown }).error)) {
    throw new Error(apiError(data, r.status));
  }
  return data as T;
}

export function apiGet<T = Record<string, unknown>>(path: string) {
  return api<T>(path);
}

export function apiPost<T = Record<string, unknown>>(path: string, body: unknown) {
  return api<T>(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

export function apiPut<T = Record<string, unknown>>(path: string, body: unknown) {
  return api<T>(path, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}
