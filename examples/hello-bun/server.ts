// No Dockerfile: hedgehog builds this with Railpack. It counts visits in the
// Postgres database hedgehog creates for it, and echoes WebSocket messages.
import { SQL } from "bun";

const db = process.env.DATABASE_URL ? new SQL(process.env.DATABASE_URL) : null;
if (db) await db`CREATE TABLE IF NOT EXISTS visits (id serial PRIMARY KEY, at timestamptz DEFAULT now())`;

const server = Bun.serve({
  port: Number(process.env.PORT ?? 3000),
  hostname: "0.0.0.0",
  async fetch(req, server) {
    const url = new URL(req.url);
    if (url.pathname === "/ws" && server.upgrade(req)) return;
    if (url.pathname === "/health") return new Response("ok");
    let visits = "no database";
    if (db) {
      await db`INSERT INTO visits DEFAULT VALUES`;
      const [row] = await db`SELECT count(*)::int AS count FROM visits`;
      visits = String(row.count);
    }
    return new Response(`<h1>Hello from hedgehog 🦔</h1><p>Visits: ${visits}</p>`, {
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
  websocket: {
    message(ws, message) {
      ws.send(`echo: ${message}`);
    },
  },
});

console.log(`listening on ${server.port}`);
