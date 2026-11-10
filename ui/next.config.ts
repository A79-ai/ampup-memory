import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // A self-contained server for the Kubernetes gateway's image (Dockerfile); Vercel builds as usual.
  output: "standalone",
  /* config options here */
  cacheComponents: true,
  partialPrefetching: true,
  turbopack: {
    rules: {
      "*.css": {
        loaders: ["@tailwindcss/turbopack"],
        as: "*.css",
      },
    },
  },
};

export default nextConfig;
