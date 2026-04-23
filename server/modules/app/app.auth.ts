import type { FastifyRequest, FastifyReply } from "fastify";
import jwt from "jsonwebtoken";

function getJwtSecret(): string {
  const secret = process.env.JWT_SECRET;
  if (!secret) {
    if (process.env.NODE_ENV === "production") {
      throw new Error("FATAL: JWT_SECRET environment variable is required in production");
    }
    console.warn("[app.auth] JWT_SECRET env var not set. Using development fallback. Set JWT_SECRET for production!");
    return "transity-dev-" + (process.env.REPL_ID || "local-dev-only");
  }
  return secret;
}

const JWT_SECRET = getJwtSecret();

export interface AppUserPayload {
  userId: string;
  email: string;
}

const JWT_TTL = process.env.JWT_TTL || "24h";

export function signToken(payload: AppUserPayload): string {
  return jwt.sign(payload, JWT_SECRET, { expiresIn: JWT_TTL as jwt.SignOptions["expiresIn"] });
}

export function verifyToken(token: string): AppUserPayload {
  return jwt.verify(token, JWT_SECRET) as AppUserPayload;
}

export async function appAuthMiddleware(req: FastifyRequest, reply: FastifyReply): Promise<void> {
  const authHeader = req.headers.authorization;
  if (!authHeader || !authHeader.startsWith("Bearer ")) {
    return reply.code(401).send({ error: "Missing or invalid authorization header" });
  }

  const token = authHeader.slice(7);
  try {
    const payload = verifyToken(token);
    req.appUser = payload;
  } catch {
    return reply.code(401).send({ error: "Invalid or expired token" });
  }
}

export async function optionalAuthMiddleware(req: FastifyRequest, _reply: FastifyReply): Promise<void> {
  const authHeader = req.headers.authorization;
  if (authHeader && authHeader.startsWith("Bearer ")) {
    try {
      const token = authHeader.slice(7);
      req.appUser = verifyToken(token);
    } catch {
    }
  }
}
