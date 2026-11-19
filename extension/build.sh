#!/bin/sh
# Builds the extension for Firefox and Chromium-based browsers into
# extension/dist/{firefox,chrome}. Needs only a POSIX shell and python3
# (for merging the manifest).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
for target in firefox chrome; do
    out="$here/dist/$target"
    rm -rf "$out"
    mkdir -p "$out"
    cp "$here"/src/* "$out"/
    python3 - "$here/manifest.base.json" "$target" > "$out/manifest.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
if sys.argv[2] == "firefox":
    manifest["background"] = {"scripts": ["common.js", "background.js"]}
    manifest["browser_specific_settings"] = {"gecko": {"id": "{70e8535c-40cf-424f-907d-a682e59d03b4}", "strict_min_version": "121.0"}}
else:
    manifest["background"] = {"service_worker": "background.js"}
    manifest["minimum_chrome_version"] = "116"
print(json.dumps(manifest, indent=2))
PY
    echo "built $out"
done
