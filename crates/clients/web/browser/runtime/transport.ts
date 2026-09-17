export class UnknownDeliveryError extends Error {
  readonly outcome = "unknown" as const;
}

export async function getJson(url: string, signal?: AbortSignal): Promise<unknown> {
  const response = await fetch(url, {
    credentials: "same-origin",
    ...(signal ? { signal } : {}),
  });
  if (!response.ok) throw new Error(`${url}: ${response.status} ${await response.text()}`);
  return response.json() as Promise<unknown>;
}

export async function postJson(url: string, body: unknown, signal?: AbortSignal): Promise<unknown> {
  let response: Response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body ?? {}),
      credentials: "same-origin",
      ...(signal ? { signal } : {}),
    });
  } catch (error) {
    throw new UnknownDeliveryError(`${url}: delivery outcome is unknown: ${errorMessage(error)}`);
  }
  if (!response.ok) throw new Error(`${url}: ${response.status} ${await response.text()}`);
  try {
    return await response.json() as unknown;
  } catch (error) {
    throw new UnknownDeliveryError(`${url}: response outcome is unknown: ${errorMessage(error)}`);
  }
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
