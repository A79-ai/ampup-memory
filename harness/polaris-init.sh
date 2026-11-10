#!/bin/sh
# Create the `memstore` catalog on the local S3 store and give the root principal full access to it.
# S3_ENDPOINT is the store as the engine reaches it: localhost from the laptop, s3 inside a cluster.
# Deleting memory (`org delete`, `workspace delete`, `users forget`) drops tables with purge and rewrites a
# table by putting a new one at the old one's location; Polaris refuses both unless the catalog allows them.
set -e
P=http://polaris:8181
S3=${S3_ENDPOINT:-http://localhost:9000}
TOKEN=$(curl -sf -X POST $P/api/catalog/v1/oauth/tokens \
  -d grant_type=client_credentials -d client_id=root -d client_secret=s3cr3t \
  -d scope=PRINCIPAL_ROLE:ALL | sed -E 's/.*"access_token":"([^"]+)".*/\1/')
H="Authorization: Bearer $TOKEN"
curl -s -X POST $P/api/management/v1/catalogs -H "$H" -H 'Content-Type: application/json' -d '{
  "catalog": {
    "name": "memstore", "type": "INTERNAL",
    "properties": {"default-base-location": "s3://warehouse/memstore",
                   "polaris.config.drop-with-purge.enabled": "true",
                   "polaris.config.allow.overlapping.table.location": "true"},
    "storageConfigInfo": {
      "storageType": "S3", "allowedLocations": ["s3://warehouse/"],
      "endpoint": "'"$S3"'", "endpointInternal": "http://s3:9000",
      "pathStyleAccess": true, "stsUnavailable": true
    }
  }
}'
curl -sf -X PUT $P/api/management/v1/catalogs/memstore/catalog-roles/catalog_admin/grants \
  -H "$H" -H 'Content-Type: application/json' \
  -d '{"grant": {"type": "catalog", "privilege": "CATALOG_MANAGE_CONTENT"}}'
curl -sf -X PUT $P/api/management/v1/principal-roles/service_admin/catalog-roles/memstore \
  -H "$H" -H 'Content-Type: application/json' \
  -d '{"catalogRole": {"name": "catalog_admin"}}'
echo "polaris catalog memstore ready"
