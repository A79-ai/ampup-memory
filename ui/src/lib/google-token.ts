import { createRemoteJWKSet, jwtVerify, type JWTVerifyGetKey } from "jose";

// Checks a Google ID token as the Kubernetes gateway accepts it: signed by Google, minted for the UI's OAuth client,
// for a verified email in an allowed domain. No import of server-only, so tests can run it directly.

const GOOGLE_JWKS = createRemoteJWKSet(new URL("https://www.googleapis.com/oauth2/v3/certs"));
const ISSUERS = ["https://accounts.google.com", "accounts.google.com"];

export type Refusal = { status: 401 | 403; reason: string };

/** The caller's email, or why they are refused: 401 for a missing or invalid token, 403 for a valid one whose
 * account isn't allowed. */
export async function verifyGoogle(
  authorization: string | null,
  opts: { audience: string; domains: string[]; keys?: JWTVerifyGetKey; issuers?: string[] },
): Promise<{ email: string } | Refusal> {
  const token = /^bearer\s+(.+)$/i.exec(authorization ?? "")?.[1]?.trim();
  if (!token) return { status: 401, reason: "no credential" };
  let claims: Record<string, unknown>;
  try {
    ({ payload: claims } = await jwtVerify(token, opts.keys ?? GOOGLE_JWKS, {
      issuer: opts.issuers ?? ISSUERS,
      audience: opts.audience,
    }));
  } catch {
    return { status: 401, reason: "invalid token" };
  }
  const email = typeof claims.email === "string" ? claims.email.toLowerCase() : "";
  if (!email || claims.email_verified !== true) return { status: 403, reason: "no verified email" };
  const domain = email.split("@").pop() ?? "";
  if (!opts.domains.map((d) => d.toLowerCase()).includes(domain)) {
    return { status: 403, reason: `${domain} may not use this gateway` };
  }
  return { email };
}

/** How the gateway's door is set up from the environment: checked (an audience and domains), open (not in a
 * cluster: a laptop, or Vercel), or closed to everyone. Inside a cluster this app only ever runs as the gateway,
 * so a missing or misspelled setting there closes it instead of opening it. */
export function door(
  env: Record<string, string | undefined>,
): { check: { audience: string; domains: string[] } } | "open" | { closed: string } {
  const audience = env.GATEWAY_AUDIENCE?.trim();
  const domains = (env.AUTH_ALLOWED_DOMAINS ?? "")
    .split(",")
    .map((d) => d.trim())
    .filter(Boolean);
  if (!audience) return env.KUBERNETES_SERVICE_HOST ? { closed: "the gateway has no GATEWAY_AUDIENCE" } : "open";
  if (!domains.length) return { closed: "the gateway has no AUTH_ALLOWED_DOMAINS" };
  return { check: { audience, domains } };
}
