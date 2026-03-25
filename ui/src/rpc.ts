/** Low-level JSON-RPC client */

let _id = 1;

export async function rpcCall(
  url: string,
  method: string,
  params: unknown[] = [],
): Promise<unknown> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", method, params, id: _id++ }),
  });
  const data = (await res.json()) as {
    result?: unknown;
    error?: { message: string };
  };
  if (data.error) throw new Error(data.error.message);
  return data.result;
}
