// The Kubernetes gateway's door: who gets in with which Google ID token. A stand-in for Google signs the tokens;
// the check under test is the gateway's own (verifyGoogle), as it runs in the cluster.
//
//   npm test

import assert from "node:assert/strict";
import { test } from "node:test";
import { SignJWT, createLocalJWKSet, exportJWK, generateKeyPair } from "jose";
import { door, verifyGoogle } from "./google-token.ts";

const AUDIENCE = "ui-client.apps.googleusercontent.com";
const google = await generateKeyPair("RS256");
const stranger = await generateKeyPair("RS256");
const jwk = { ...(await exportJWK(google.publicKey)), kid: "g1", alg: "RS256" };
const keys = createLocalJWKSet({ keys: [jwk] });
const opts = { audience: AUDIENCE, domains: ["example.com", "example.org"], keys };

async function token(claims: Record<string, unknown>, o: { key?: CryptoKey; aud?: string; iss?: string; exp?: string } = {}) {
  return new SignJWT({ email_verified: true, ...claims })
    .setProtectedHeader({ alg: "RS256", kid: "g1" })
    .setIssuer(o.iss ?? "https://accounts.google.com")
    .setAudience(o.aud ?? AUDIENCE)
    .setIssuedAt()
    .setExpirationTime(o.exp ?? "1h")
    .sign(o.key ?? google.privateKey);
}
const bearer = (t: string) => `Bearer ${t}`;
const status = (r: Awaited<ReturnType<typeof verifyGoogle>>) => ("status" in r ? r.status : 200);

test("a verified account in an allowed domain with a token for the UI's client gets in", async () => {
  assert.deepEqual(await verifyGoogle(bearer(await token({ email: "alice@example.com" })), opts), {
    email: "alice@example.com",
  });
  assert.deepEqual(await verifyGoogle(`bearer ${await token({ email: "Bob@Example.org" })}`, opts), {
    email: "bob@example.org",
  });
});

test("no credential, or not a bearer token: 401", async () => {
  assert.equal(status(await verifyGoogle(null, opts)), 401);
  assert.equal(status(await verifyGoogle("", opts)), 401);
  assert.equal(status(await verifyGoogle(`Basic ${await token({ email: "a@example.com" })}`, opts)), 401);
});

test("a token for another app, from another issuer, expired, or not signed by Google: 401", async () => {
  const email = { email: "alice@example.com" };
  for (const t of [
    await token(email, { aud: "someone-elses-client" }),
    await token(email, { iss: "https://evil.example" }),
    await token(email, { exp: "-1m" }),
    await token(email, { key: stranger.privateKey }),
    "not.a.jwt",
  ]) {
    const r = await verifyGoogle(bearer(t), opts);
    assert.equal(status(r), 401, JSON.stringify(r));
  }
});

test("a valid token whose account isn't allowed: 403", async () => {
  for (const claims of [
    { email: "someone@gmail.com" },
    { email: "alice@example.com.evil.com" },
    { email: "alice@example.com", email_verified: false },
    { sub: "no-email" },
  ]) {
    const r = await verifyGoogle(bearer(await token(claims)), opts);
    assert.equal(status(r), 403, JSON.stringify(r));
  }
});

test("in a cluster the gateway is never open: without its settings it refuses everyone", async () => {
  const cluster = { KUBERNETES_SERVICE_HOST: "10.0.0.1" };
  assert.deepEqual(door({ ...cluster, AUTH_ALLOWED_DOMAINS: "example.com" }), { closed: "the gateway has no GATEWAY_AUDIENCE" });
  // The deploy bug that left it open: the audience under a mangled name.
  assert.ok("closed" in (door({ ...cluster, "GATEWAY_123456-x.apps.googleusercontent.com": "AUDIENCE" }) as object));
  assert.ok("closed" in (door({ ...cluster, GATEWAY_AUDIENCE: AUDIENCE }) as object));
  assert.deepEqual(door({ ...cluster, GATEWAY_AUDIENCE: AUDIENCE, AUTH_ALLOWED_DOMAINS: "example.com, example.org" }), {
    check: { audience: AUDIENCE, domains: ["example.com", "example.org"] },
  });
  // Not in a cluster (a laptop, Vercel) and not configured as the gateway: no door.
  assert.equal(door({}), "open");
});
