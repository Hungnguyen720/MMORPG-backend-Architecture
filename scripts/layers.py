"""Print the world service's /status (JSON on stdin) on one line."""
import json
import sys

layers = json.load(sys.stdin)
parts = []
for l in layers:
    flag = " (draining)" if l["draining"] else ""
    parts.append(f"{l['zone']}#{l['layer']}={l['players']}{flag}")
print("  ".join(parts) or "no layers")
