# Shared AWS modules

Used by [`../byoc`](../byoc) and [`../byovpc`](../byovpc); not applied on their own.

- `runtime`: the engine and agent roles (EKS Pod Identity), the EKS cluster and node roles, and the memory
  bucket.
- `vendor`: the management role the vendor assumes with its external id, and its three policies (network, EKS,
  ops); the network policy differs between BYOC (`create_network = true`) and BYOVPC.
