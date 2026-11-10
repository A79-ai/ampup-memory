import NextAuth from "next-auth";
import { authOptions } from "@/lib/auth";
import { GATEWAY_AUDIENCE } from "@/lib/gateway";

const handler = NextAuth(authOptions);
// The gateway signs no one in: it only checks the tokens the hosted UI sends.
const off = () => Response.json({ error: "not here" }, { status: 404 });
export const GET = GATEWAY_AUDIENCE ? off : handler;
export const POST = GATEWAY_AUDIENCE ? off : handler;
