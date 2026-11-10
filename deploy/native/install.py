"""Install the Memory app in your own Snowflake account (you are provider and consumer, so no
Marketplace listing or provider approval is needed).

Needs: a Snowflake account on AWS, a role that can create databases, application packages and
applications (ACCOUNTADMIN is simplest), Docker, and the memstore image (a registry reference you
can pull, or a `docker save` tarball).

    uv run --with snowflake-connector-python python3 deploy/native/install.py \\
        --connection <name in ~/.snowflake/connections.toml> --image <ref or image.tar> [--app MEMORY_APP]

Then run consumer_setup.sql (with MEMORY_APP replaced by your app name) in a Snowsight worksheet.
"""

import argparse
import os
import subprocess
from pathlib import Path

import snowflake.connector

APP_DIR = Path(__file__).parent / "app"
SRC_DB = "MEMSTORE_PKG_SRC"  # the manifest's image path is /memstore_pkg_src/public/images/...
IMAGE = "memstore:v0_1"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--connection", required=True)
    ap.add_argument("--image", required=True, help="registry reference or docker-save tarball")
    ap.add_argument("--app", default="MEMORY_APP")
    ap.add_argument("--package", default="MEMORY_PKG")
    a = ap.parse_args()

    db = snowflake.connector.connect(connection_name=a.connection)
    sql = lambda q: db.cursor().execute(q).fetchall()  # noqa: E731
    version = next(l.split()[1] for l in (APP_DIR / "manifest.yml").read_text().splitlines()
                   if l.strip().startswith("name:"))

    # 1. The image, in this account's registry at the path the manifest names.
    sql(f"CREATE DATABASE IF NOT EXISTS {SRC_DB}")
    sql(f"CREATE IMAGE REPOSITORY IF NOT EXISTS {SRC_DB}.PUBLIC.IMAGES")
    sql(f"CREATE STAGE IF NOT EXISTS {SRC_DB}.PUBLIC.{version}")
    repo_url = next(r[4] for r in sql(f"SHOW IMAGE REPOSITORIES IN SCHEMA {SRC_DB}.PUBLIC") if r[1] == "IMAGES")
    target = f"{repo_url}/{IMAGE}"
    if Path(a.image).is_file():
        out = subprocess.run(["docker", "load", "-i", a.image], check=True, capture_output=True, text=True).stdout
        source = out.strip().split("Loaded image: ")[-1]
    else:
        subprocess.run(["docker", "pull", "--platform", "linux/amd64", a.image], check=True)
        source = a.image
    subprocess.run(["docker", "tag", source, target], check=True)
    registry = repo_url.split("/")[0]
    if os.environ.get("SNOWFLAKE_PAT"):  # non-interactive: log in with a PAT from the environment
        subprocess.run(["docker", "login", registry, "-u", db.user, "--password-stdin"],
                       input=os.environ["SNOWFLAKE_PAT"], text=True, check=True)
    else:
        print(f"docker login to {registry} (your Snowflake user; a PAT if you sign in with SSO):")
        subprocess.run(["docker", "login", registry], check=True)
    subprocess.run(["docker", "push", target], check=True)

    # 2. The package files, then the package, a version and the application.
    stage = f"@{SRC_DB}.PUBLIC.{version}"
    for f in ["manifest.yml", "setup.sql", "README.md"]:
        sql(f"PUT file://{(APP_DIR / f).resolve()} {stage} AUTO_COMPRESS=FALSE OVERWRITE=TRUE")
    for f in sorted((APP_DIR / "service").glob("*.yaml")):
        sql(f"PUT file://{f.resolve()} {stage}/service AUTO_COMPRESS=FALSE OVERWRITE=TRUE")
    sql(f"CREATE APPLICATION PACKAGE IF NOT EXISTS {a.package}")
    if not any(r[0] == version for r in sql(f"SHOW VERSIONS IN APPLICATION PACKAGE {a.package}")):
        sql(f"ALTER APPLICATION PACKAGE {a.package} REGISTER VERSION {version} USING '{stage}'")
    else:
        sql(f"ALTER APPLICATION PACKAGE {a.package} ADD PATCH FOR VERSION {version} USING '{stage}'")
    if any(r[1] == a.app for r in sql(f"SHOW APPLICATIONS LIKE '{a.app}'")):
        sql(f"ALTER APPLICATION {a.app} UPGRADE USING VERSION {version}")
    else:
        sql(f"CREATE APPLICATION {a.app} FROM APPLICATION PACKAGE {a.package} USING VERSION {version}")
    print(f"\nInstalled {a.app} ({version}). Next: run deploy/native/consumer_setup.sql in a Snowsight "
          f"worksheet with MEMORY_APP replaced by {a.app}, approve STORAGE_ACCESS, then see the app's README.")


if __name__ == "__main__":
    main()
