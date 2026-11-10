"use client";

import { SessionProvider } from "next-auth/react";

// The session is polled every few minutes, which is also when the server renews the Google ID token it holds.
export function Session({ children }: { children: React.ReactNode }) {
  return <SessionProvider refetchInterval={5 * 60}>{children}</SessionProvider>;
}
