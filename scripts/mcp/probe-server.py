#!/usr/bin/env python3
"""A harmless MCP server used to verify real provider connections and tool calls."""
import argparse
import json
import os
import sys
import time
from pathlib import Path
from datetime import datetime, timezone

parser = argparse.ArgumentParser()
parser.add_argument('--marker', required=True)
parser.add_argument('--label', required=True)
args = parser.parse_args()
marker = Path(args.marker)
marker.parent.mkdir(parents=True, exist_ok=True)

def record(method, tool=None):
    with marker.open('a', encoding='utf-8') as file:
        file.write(json.dumps({'method': method, 'label': args.label, 'tool': tool, 'pid': os.getpid(), 'at': datetime.now(timezone.utc).isoformat()}) + '\n')

for line in sys.stdin:
    packet = json.loads(line)
    method = packet.get('method', '')
    record(method, packet.get('params', {}).get('name'))
    if 'id' not in packet:
        continue
    if method == 'initialize':
        result = {'protocolVersion': packet.get('params', {}).get('protocolVersion', '2024-11-05'), 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'yi-mcp-probe', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'yi_mcp_probe', 'description': 'Return a random verification nonce from this harmless test server.', 'inputSchema': {'type': 'object', 'properties': {'delayMs': {'type': 'integer', 'minimum': 0, 'maximum': 3000}}, 'additionalProperties': False}, 'annotations': {'readOnlyHint': True, 'destructiveHint': False, 'openWorldHint': False}}]}
    elif method == 'tools/call' and packet.get('params', {}).get('name') == 'yi_mcp_probe':
        time.sleep(min(3000, max(0, packet.get('params', {}).get('arguments', {}).get('delayMs', 0))) / 1000)
        result = {'content': [{'type': 'text', 'text': json.dumps({'nonce': os.environ['YI_MCP_TEST_NONCE'], 'provider': args.label})}]}
    elif method in ('resources/list', 'resources/templates/list', 'prompts/list'):
        result = {'resources': [], 'resourceTemplates': [], 'prompts': []}
    elif method == 'ping':
        result = {}
    else:
        print(json.dumps({'jsonrpc': '2.0', 'id': packet['id'], 'error': {'code': -32601, 'message': 'Unknown probe method'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': packet['id'], 'result': result}), flush=True)
