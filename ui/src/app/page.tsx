import { connection } from "next/server";
import { Suspense } from "react";
import { Browser } from "@/components/browser";
import { Session } from "@/components/session";

export default function Home() {
  return (
    <Suspense>
      <Page />
    </Suspense>
  );
}

async function Page() {
  // Read at request time: the same build runs as the UI and as the Kubernetes gateway.
  await connection();
  if (process.env.GATEWAY_AUDIENCE) {
    return <p style={{ padding: 24 }}>memstore Kubernetes gateway: API only, for the memory UI.</p>;
  }
  return (
    <Session>
      <Browser />
    </Session>
  );
}
