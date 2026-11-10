# Delete one fact row from outside the engine (as another Iceberg engine or an erasure job
# would), through the harness Polaris catalog. Usage: delete_fact.py <namespace> <fact_id>
import sys
from pyiceberg.catalog import load_catalog

ns, fact_id = sys.argv[1], sys.argv[2]
cat = load_catalog("polaris", **{
    "type": "rest", "uri": "http://localhost:8181/api/catalog", "warehouse": "memstore",
    "credential": "root:s3cr3t", "scope": "PRINCIPAL_ROLE:ALL",
    "s3.endpoint": "http://localhost:9000", "s3.access-key-id": "admin",
    "s3.secret-access-key": "password", "s3.region": "us-east-1",
    # The harness stack has no STS, so ask for no vended credentials; use the keys above.
    "header.X-Iceberg-Access-Delegation": "",
})
t = cat.load_table(f"{ns}.facts")
before = len(t.scan(row_filter=f"fact_id == '{fact_id}'").to_arrow())
t.delete(f"fact_id == '{fact_id}'")
after = len(t.refresh().scan(row_filter=f"fact_id == '{fact_id}'").to_arrow())
print(f"deleted {fact_id} from {ns}.facts: rows {before} -> {after}")
