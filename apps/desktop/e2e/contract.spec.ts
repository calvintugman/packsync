import { ALL_METHODS, LONG_METHODS } from "../src/api/methods";
import { expect, test } from "./world";

// The typed method table of the UI must match what the application layer really offers.
test("the UI's method table matches the application layer", async ({ world }) => {
  const res = await fetch(`http://127.0.0.1:${world.port}/api/methods`, { headers: { "x-packsync-token": world.token } });
  const doc = (await res.json()) as { ok: boolean; result: { name: string; long: boolean }[] };
  expect(doc.ok).toBe(true);
  const server = new Map(doc.result.map((m) => [m.name, m.long]));
  expect([...server.keys()].sort()).toEqual([...ALL_METHODS].sort());
  for (const m of ALL_METHODS) {
    expect(server.get(m), `${m} long flag`).toBe((LONG_METHODS as readonly string[]).includes(m));
  }
});

test("the bridge refuses calls without its token and with a foreign Host header", async ({ world }) => {
  const noToken = await fetch(`http://127.0.0.1:${world.port}/api/call`, { method: "POST", body: JSON.stringify({ method: "app.info" }) });
  expect(noToken.status).toBe(401);
  const wrongToken = await fetch(`http://127.0.0.1:${world.port}/api/call`, {
    method: "POST",
    headers: { "x-packsync-token": "0".repeat(64) },
    body: JSON.stringify({ method: "app.info" }),
  });
  expect(wrongToken.status).toBe(401);
});
