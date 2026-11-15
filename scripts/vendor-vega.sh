#!/usr/bin/env bash
# Re-download the vendored chart libraries into web/static/vendor/.
# Usage: scripts/vendor-vega.sh  (edit the versions below to upgrade, then run the tests)
set -euo pipefail
cd "$(dirname "$0")/../web/static/vendor"

VEGA=6.4.0 VEGA_LITE=6.5.0 VEGA_EMBED=7.3.0 VEGA_INTERPRETER=2.3.2
cdn=https://cdn.jsdelivr.net/npm

curl -fsSL "$cdn/vega@$VEGA/build/vega.min.js" -o vega.min.js
curl -fsSL "$cdn/vega-lite@$VEGA_LITE/build/vega-lite.min.js" -o vega-lite.min.js
curl -fsSL "$cdn/vega-embed@$VEGA_EMBED/build/vega-embed.min.js" -o vega-embed.min.js
curl -fsSL "$cdn/vega-interpreter@$VEGA_INTERPRETER/build/vega-interpreter.min.js" -o vega-interpreter.min.js

# vega-interpreter ships only as an ES module. The admin UI loads classic scripts (no
# bundler), so repackage it: take its vega-util imports from the `vega` global and
# publish the export as `window.vegaInterpreter`.
VEGA_INTERPRETER="$VEGA_INTERPRETER" python3 - <<'PY'
import os, re
version = os.environ["VEGA_INTERPRETER"]
s = open("vega-interpreter.min.js").read()
start = s.index("import{")
end = s.index(";", start) + 1
names = re.fullmatch(r'import\{([\w,]+)\}from"vega-util";', s[start:end])
assert names, "unexpected import line: " + s[start:end]
export = "export{expression as expressionInterpreter};"
assert export in s, "unexpected export line"
body = re.sub(r"\n?//# sourceMappingURL=\S*\s*$", "", s[end:].replace(export, "").rstrip())
imports = ",".join(f"{n}=vega.{n}" for n in names.group(1).split(","))
open("vega-interpreter.min.js", "w").write(
    f"/* vega-interpreter {version} (BSD-3-Clause), repackaged as a classic script by\n"
    "   scripts/vendor-vega.sh: imports come from the `vega` global, the export is\n"
    "   `window.vegaInterpreter`. */\n"
    f'(function(){{"use strict";var {imports};{body}\n'
    "window.vegaInterpreter={expressionInterpreter:expression};})();\n"
)
PY

cat > VERSIONS.txt <<TXT
Vendored front-end libraries (no CDN at runtime). Regenerate the Vega files with
scripts/vendor-vega.sh.

htmx.org 2.0.4              htmx.min.js
vega $VEGA                  vega.min.js
vega-lite $VEGA_LITE             vega-lite.min.js
vega-embed $VEGA_EMBED            vega-embed.min.js
vega-interpreter $VEGA_INTERPRETER      vega-interpreter.min.js (repackaged, see the script)
TXT
echo "vendored: vega $VEGA, vega-lite $VEGA_LITE, vega-embed $VEGA_EMBED, vega-interpreter $VEGA_INTERPRETER"
