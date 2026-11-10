import "server-only";

import type { NextAuthOptions } from "next-auth";
import { getToken } from "next-auth/jwt";
import GoogleProvider from "next-auth/providers/google";

// Google sign-in, for a UI hosted against one memstore (`MEMSTORE_URL`, e.g. on Vercel). Each request to
// memstore carries the signed-in person's own Google ID token, so memstore decides what they may read, as for
// any other caller. The token stays in the encrypted session cookie and is read only on the server: the
// browser's session holds just the email.
//
//   AUTH_GOOGLE_ID, AUTH_GOOGLE_SECRET  the OAuth client (a Web application, redirect URI
//                                       <site>/api/auth/callback/google)
//   NEXTAUTH_SECRET                     encrypts the session cookie
//   AUTH_ALLOWED_DOMAINS                optional: only these email domains may sign in (e.g. example.com)

const allowed = (process.env.AUTH_ALLOWED_DOMAINS ?? "")
  .split(",")
  .map((d) => d.trim().toLowerCase())
  .filter(Boolean);

type Google = { idToken: string; refreshToken?: string; expiresAt: number };

/** A fresh ID token from the refresh token, a minute before the old one ends. */
async function refreshed(t: Google): Promise<Google> {
  if (Date.now() < t.expiresAt - 60_000 || !t.refreshToken) return t;
  const r = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: t.refreshToken,
      client_id: process.env.AUTH_GOOGLE_ID ?? "",
      client_secret: process.env.AUTH_GOOGLE_SECRET ?? "",
    }),
  });
  const v = await r.json();
  if (!r.ok || !v.id_token) throw new Error(`refreshing the Google sign-in failed: ${v.error ?? r.status}`);
  return { idToken: v.id_token, refreshToken: t.refreshToken, expiresAt: Date.now() + v.expires_in * 1000 };
}

export const authOptions: NextAuthOptions = {
  providers: [
    GoogleProvider({
      clientId: process.env.AUTH_GOOGLE_ID ?? "",
      clientSecret: process.env.AUTH_GOOGLE_SECRET ?? "",
      // A refresh token, so the ID token can be renewed without signing in again every hour.
      authorization: { params: { access_type: "offline", prompt: "consent", scope: "openid email profile" } },
    }),
  ],
  session: { strategy: "jwt" },
  callbacks: {
    signIn({ profile }) {
      const p = profile as { email?: string; email_verified?: boolean } | undefined;
      if (!p?.email || p.email_verified === false) return false;
      const domain = p.email.split("@").pop()?.toLowerCase() ?? "";
      return allowed.length === 0 || allowed.includes(domain);
    },
    async jwt({ token, account }) {
      if (account?.id_token) {
        token.google = {
          idToken: account.id_token,
          refreshToken: account.refresh_token,
          expiresAt: (account.expires_at ?? 0) * 1000,
        } satisfies Google;
      } else if (token.google) {
        // Renewed here, so the new token is written back to the cookie (the page polls the session).
        try {
          token.google = await refreshed(token.google as Google);
        } catch {
          delete token.google; // signed out: the page asks them to sign in again
        }
      }
      return token;
    },
    // What the browser sees: who is signed in, never the token.
    session({ session }) {
      return session;
    },
  },
};

/** The signed-in person's Google ID token for memstore, renewed if it is about to end; null when signed out. */
export async function idToken(req: Request): Promise<string | null> {
  const token = await getToken({
    req: req as unknown as Parameters<typeof getToken>[0]["req"],
    secret: process.env.NEXTAUTH_SECRET,
  });
  const g = token?.google as Google | undefined;
  if (!g) return null;
  return (await refreshed(g)).idToken;
}
