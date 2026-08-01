#!/usr/bin/env python3
"""Extract Ironjaw RPC method schema from handler.rs source code.

Parses the method dispatch match block to produce a JSON schema file
that can be consumed by the Kotlin code generator.

Usage:
    python extract_schema.py > schema.json
"""
import json
import re
import sys
from pathlib import Path

HANDLER_RS = Path(__file__).parent.parent.parent / "crates" / "ironjaw-ws" / "src" / "handler.rs"
TIERS_RS = Path(__file__).parent.parent.parent / "crates" / "ironjaw-security" / "src" / "method_tiers.rs"


def extract_methods(handler_src: str) -> list[dict]:
    """Extract method names from handler.rs dispatch block."""
    # Match patterns like: "method.name" => {
    pattern = re.compile(r'"([a-z][a-z0-9]*\.[a-z][a-z0-9_.]*)"(?:\s*\|[^=]*)?\s*=>')
    methods = []
    seen = set()

    for match in pattern.finditer(handler_src):
        method = match.group(1)
        if method not in seen and not method.startswith("permission."):
            seen.add(method)
            methods.append({"method": method, "line": handler_src[:match.start()].count('\n') + 1})

    return methods


def extract_tiers(tiers_src: str) -> dict[str, str]:
    """Extract method-to-tier mapping from method_tiers.rs."""
    tier_map = {}
    # Match: "method.name" => PermissionTier::TierName,
    pattern = re.compile(r'"([^"]+)"\s*=>\s*PermissionTier::(\w+)')
    for match in pattern.finditer(tiers_src):
        tier_map[match.group(1)] = match.group(2).lower()
    return tier_map


def categorize_method(method: str) -> str:
    """Group methods by namespace."""
    parts = method.split(".")
    return parts[0]


def main():
    if not HANDLER_RS.exists():
        print(f"Error: {HANDLER_RS} not found", file=sys.stderr)
        sys.exit(1)

    handler_src = HANDLER_RS.read_text(encoding="utf-8")
    methods = extract_methods(handler_src)

    tier_map = {}
    if TIERS_RS.exists():
        tier_map = extract_tiers(TIERS_RS.read_text(encoding="utf-8"))

    schema = {
        "$schema": "ironjaw-rpc-schema/1.0",
        "generated": True,
        "methods": [],
    }

    for m in methods:
        method = m["method"]
        category = categorize_method(method)
        tier = tier_map.get(method, "unknown")

        schema["methods"].append({
            "method": method,
            "category": category,
            "tier": tier,
            "line": m["line"],
        })

    json.dump(schema, sys.stdout, indent=2)
    print()


if __name__ == "__main__":
    main()
