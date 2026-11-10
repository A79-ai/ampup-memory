# Shared GCP modules

Used by [`../byoc`](../byoc) and [`../byovpc`](../byovpc); not applied on their own.

- `runtime`: the engine, ingest, agent and node service accounts with their grants, the memory bucket and the
  BigQuery dataset.
- `vendor`: the management service account the vendor impersonates, its custom roles, and the network grants
  (`create_network = true` for BYOC; the customer's subnet for BYOVPC).
