#!/usr/bin/env python3
"""Stands in for `prime-agent send`: appends each call to $FAKE_AGENT_LOG. Sessions named dead-* fail."""
import json
import os
import sys

args = sys.argv[1:]
assert args[:2] == ["send", "--json"] and args[3] == "--", args
session, text = args[2], args[4]
if session.startswith("dead"):
    print(f"Error: Unknown active session: {session}", file=sys.stderr)
    sys.exit(1)
with open(os.environ["FAKE_AGENT_LOG"], "a") as f:
    f.write(json.dumps({"session": session, "text": text}) + "\n")
print(json.dumps({"deliveryStatus": "delivered"}))
