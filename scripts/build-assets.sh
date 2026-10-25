#!/usr/bin/env bash
# Package a tenant's asset pack: tenants/<id>/assets/tenant/** into
# build/<id>-assets.zip. The client mounts it at res:// with
# ProjectSettings.load_resource_pack, so models resolve as
# res://tenant/models/<tag>.tscn and icons as res://tenant/icons/<name>.png.
set -euo pipefail
cd "$(dirname "$0")/.."
id=${1:?usage: build-assets.sh <tenant id>}
src=tenants/$id/assets
[[ -d $src/tenant ]] || { echo "no $src/tenant directory"; exit 1; }
mkdir -p build
out=$PWD/build/$id-assets.zip
rm -f "$out"
(cd "$src" && python3 - "$out" <<'PY'
import os, sys, zipfile
out = sys.argv[1]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for root, _, files in os.walk("tenant"):
        for f in sorted(files):
            z.write(os.path.join(root, f))
print(out, os.path.getsize(out), "bytes")
PY
)
