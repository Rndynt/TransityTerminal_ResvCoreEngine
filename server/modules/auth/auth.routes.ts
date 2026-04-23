import type { FastifyInstance } from "fastify";
import {
  REALMIO_BASE_URL,
  REALMIO_TENANT_ID,
  DEV_BYPASS_AUTH,
  DEV_USER,
  createRealmioUser,
} from "./realmio";
import { db } from "@server/db";
import { staffMembers, users } from "@shared/schema";

export function registerAuthRoutes(app: FastifyInstance) {
  app.post("/api/auth/sign-in/email", { config: { rateLimit: { max: 10, timeWindow: '1 minute' } } }, async (req, reply) => {
    if (DEV_BYPASS_AUTH) {
      return reply.send({
        user: DEV_USER,
        session: {
          id: "dev-session-001",
          token: "dev-token",
          userId: DEV_USER.id,
          expiresAt: new Date(Date.now() + 7 * 24 * 60 * 60 * 1000).toISOString(),
        },
      });
    }

    try {
      const upstream = await fetch(`${REALMIO_BASE_URL}/api/auth/sign-in/email`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Tenant-Id": REALMIO_TENANT_ID,
          ...(req.headers.cookie ? { Cookie: req.headers.cookie } : {}),
          ...(req.headers.origin ? { Origin: req.headers.origin } : {}),
        },
        body: JSON.stringify(req.body),
      });

      const setCookies = upstream.headers.getSetCookie?.() ?? [];
      for (const c of setCookies) {
        reply.header("Set-Cookie", c);
      }

      const data = await upstream.json();
      return reply.code(upstream.status).send(data);
    } catch (err) {
      return reply.code(502).send({ message: "Auth service unavailable" });
    }
  });

  app.post("/api/auth/sign-up/email", { config: { rateLimit: { max: 5, timeWindow: '1 minute' } } }, async (req, reply) => {
    if (DEV_BYPASS_AUTH) {
      return reply.send({
        user: { ...DEV_USER, email: (req.body as any).email, name: (req.body as any).name },
        session: {
          id: "dev-session-001",
          token: "dev-token",
          userId: DEV_USER.id,
          expiresAt: new Date(Date.now() + 7 * 24 * 60 * 60 * 1000).toISOString(),
        },
      });
    }

    try {
      const upstream = await fetch(`${REALMIO_BASE_URL}/api/auth/sign-up/email`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Tenant-Id": REALMIO_TENANT_ID,
          ...(req.headers.origin ? { Origin: req.headers.origin } : {}),
        },
        body: JSON.stringify(req.body),
      });

      const setCookies = upstream.headers.getSetCookie?.() ?? [];
      for (const c of setCookies) {
        reply.header("Set-Cookie", c);
      }

      const data = await upstream.json();
      return reply.code(upstream.status).send(data);
    } catch (err) {
      return reply.code(502).send({ message: "Auth service unavailable" });
    }
  });

  app.post("/api/auth/sign-out", async (req, reply) => {
    if (DEV_BYPASS_AUTH) {
      return reply.send({ success: true });
    }

    const clearExpiry = "Expires=Thu, 01 Jan 1970 00:00:00 GMT";

    try {
      const upstream = await fetch(`${REALMIO_BASE_URL}/api/auth/sign-out`, {
        method: "POST",
        headers: {
          "X-Tenant-Id": REALMIO_TENANT_ID,
          ...(req.headers.cookie ? { Cookie: req.headers.cookie } : {}),
        },
      });

      const setCookies = upstream.headers.getSetCookie?.() ?? [];
      for (const c of setCookies) {
        reply.header("Set-Cookie", c);
      }

      // Explicitly clear all cookies sent by the browser on our own domain,
      // since Realmio's Set-Cookie targets its own domain and won't clear ours.
      const incomingCookieNames = (req.headers.cookie ?? "")
        .split(";")
        .map((c) => c.trim().split("=")[0])
        .filter(Boolean);

      for (const name of incomingCookieNames) {
        reply.header("Set-Cookie", `${name}=; Path=/; ${clearExpiry}; HttpOnly; SameSite=Lax`);
        reply.header("Set-Cookie", `${name}=; Path=/; ${clearExpiry}; HttpOnly; SameSite=None; Secure`);
      }

      const data = await upstream.json().catch(() => ({ success: true }));
      return reply.code(upstream.ok ? upstream.status : 200).send(data);
    } catch (err) {
      // Even if Realmio is unreachable, clear cookies so the user is logged out locally
      const incomingCookieNames = (req.headers.cookie ?? "")
        .split(";")
        .map((c) => c.trim().split("=")[0])
        .filter(Boolean);

      for (const name of incomingCookieNames) {
        reply.header("Set-Cookie", `${name}=; Path=/; ${clearExpiry}; HttpOnly; SameSite=Lax`);
      }

      return reply.code(200).send({ success: true });
    }
  });

  app.get("/api/auth/session", async (req, reply) => {
    if (DEV_BYPASS_AUTH) {
      return reply.send({
        user: DEV_USER,
        session: {
          id: "dev-session-001",
          token: "dev-token",
          userId: DEV_USER.id,
          expiresAt: new Date(Date.now() + 7 * 24 * 60 * 60 * 1000).toISOString(),
        },
      });
    }

    try {
      const upstream = await fetch(`${REALMIO_BASE_URL}/api/auth/get-session`, {
        headers: {
          "X-Tenant-Id": REALMIO_TENANT_ID,
          ...(req.headers.cookie ? { Cookie: req.headers.cookie } : {}),
          ...(req.headers.authorization
            ? { Authorization: req.headers.authorization }
            : {}),
        },
      });

      if (!upstream.ok) {
        return reply.code(401).send({ user: null, session: null });
      }

      const data = await upstream.json();
      return reply.send(data);
    } catch (err) {
      return reply.code(401).send({ user: null, session: null });
    }
  });

  app.get("/api/setup/status", async (_req, reply) => {
    const rows = await db.select().from(staffMembers).limit(1);
    return reply.send({ needsSetup: rows.length === 0 });
  });

  app.post("/api/setup/init", { config: { rateLimit: { max: 5, timeWindow: '1 minute' } } }, async (req, reply) => {
    const existing = await db.select().from(staffMembers).limit(1);
    if (existing.length > 0) {
      return reply.code(403).send({ message: "Setup sudah dilakukan. Halaman ini tidak bisa diakses lagi." });
    }

    const { name, email, password } = req.body as { name?: string; email?: string; password?: string };
    if (!name || !email || !password) {
      return reply.code(400).send({ message: "name, email, dan password wajib diisi" });
    }
    if (password.length < 8) {
      return reply.code(400).send({ message: "Password minimal 8 karakter" });
    }

    let realmioUser: { userId: string; email: string; name: string };
    try {
      realmioUser = await createRealmioUser(name, email, password);
    } catch (err: any) {
      return reply.code(422).send({ message: err.message || "Gagal membuat akun di sistem autentikasi" });
    }

    const { roles } = await import("../../../shared/schema");
    const ownerRoles = await db.select().from(roles).limit(1);
    const ownerRoleId = ownerRoles.find(r => r.id === "owner")?.id ?? ownerRoles[0]?.id;

    if (!ownerRoleId) {
      return reply.code(500).send({ message: "Role 'owner' tidak ditemukan di database. Pastikan seed sudah dijalankan." });
    }

    const now = new Date();
    await db
      .insert(users)
      .values({
        id:        realmioUser.userId,
        email:     realmioUser.email,
        name:      realmioUser.name,
        createdAt: now,
        updatedAt: now,
      })
      .onConflictDoUpdate({
        target: users.id,
        set: { name: realmioUser.name, email: realmioUser.email, updatedAt: now },
      });

    const [created] = await db.insert(staffMembers).values({
      userId:   realmioUser.userId,
      roleId:   ownerRoleId,
      isActive: true,
    }).returning();

    return reply.code(201).send({ staff: created, message: "Akun owner berhasil dibuat. Silakan login." });
  });

  app.get("/api/auth/me", async (req, reply) => {
    if (DEV_BYPASS_AUTH) {
      return reply.send({
        user: DEV_USER,
        tenant: { id: REALMIO_TENANT_ID, slug: REALMIO_TENANT_ID },
      });
    }

    try {
      const upstream = await fetch(`${REALMIO_BASE_URL}/me`, {
        headers: {
          "X-Tenant-Id": REALMIO_TENANT_ID,
          ...(req.headers.cookie ? { Cookie: req.headers.cookie } : {}),
          ...(req.headers.authorization
            ? { Authorization: req.headers.authorization }
            : {}),
        },
      });

      if (!upstream.ok) {
        return reply.code(401).send({ user: null });
      }

      const data = await upstream.json();
      return reply.send(data);
    } catch (err) {
      return reply.code(401).send({ user: null });
    }
  });
}
