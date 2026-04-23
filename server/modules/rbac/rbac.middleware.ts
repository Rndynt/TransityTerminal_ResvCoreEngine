import type { FastifyRequest, FastifyReply } from "fastify";

export function requireFlag(flagId: string) {
  return async (req: FastifyRequest, reply: FastifyReply): Promise<void> => {
    if (!req.rbac || !req.rbac.flags.has(flagId)) {
      return reply.code(403).send({ error: "Forbidden", requiredFlag: flagId });
    }
  };
}

export function requireAnyFlag(...flagIds: string[]) {
  return async (req: FastifyRequest, reply: FastifyReply): Promise<void> => {
    if (!req.rbac || !flagIds.some(f => req.rbac!.flags.has(f))) {
      return reply.code(403).send({ error: "Forbidden", requiredFlags: flagIds });
    }
  };
}

export function requireOutletScope() {
  return async (req: FastifyRequest, _reply: FastifyReply): Promise<void> => {
    const outletId = req.rbac?.outletId ?? null;
    req.scopedOutletId = outletId;
    req.outletId = outletId;
  };
}
