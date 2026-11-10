# The memory UI as a gateway inside a cluster (README, "One memstore, as the signed-in person"): the Next.js
# server plus kubectl, which reaches each memstore-* namespace's memstore-serve through port-forwards with the
# pod's service account. Build with this directory as the context: docker build -t memstore-ui ui
FROM node:22-bookworm-slim AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npx next build

FROM node:22-bookworm-slim
# kubectl within one minor version of the cluster (1.35), checked against its published sha256.
ARG KUBECTL=v1.35.0
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl -fsSLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${KUBECTL}/bin/linux/amd64/kubectl" \
    && echo "$(curl -fsSL "https://dl.k8s.io/release/${KUBECTL}/bin/linux/amd64/kubectl.sha256")  /usr/local/bin/kubectl" | sha256sum -c - \
    && chmod +x /usr/local/bin/kubectl \
    && apt-get purge -y curl && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/.next/standalone ./
COPY --from=build /app/.next/static ./.next/static
COPY --from=build /app/public ./public
ENV NODE_ENV=production PORT=8080 HOSTNAME=0.0.0.0
USER node
EXPOSE 8080
CMD ["node", "server.js"]
