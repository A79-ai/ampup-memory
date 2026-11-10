# Engine settings for the standalone harness stack: `source harness/env.sh`.
export AMPUP_MEMORY_PLATFORM=rest
export AMPUP_MEMORY_REST_URI=http://localhost:8181/api/catalog
export AMPUP_MEMORY_WAREHOUSE=memstore
export AMPUP_MEMORY_REST_PROPS="credential=root:s3cr3t;scope=PRINCIPAL_ROLE:ALL;s3.endpoint=http://localhost:9000;s3.access-key-id=admin;s3.secret-access-key=password;s3.region=us-east-1;s3.path-style-access=true"
