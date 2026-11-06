#!/usr/bin/env python3
"""Prepare the pinned desktop driver.

Without arguments: the Windows x64 executable that yi-agentd embeds (runtime/computer-use/windows-x64/).
With --macos OUT: CuaDriver.app plus its license, repacked as a .tgz that the macOS installer ships next to
yi-agentd. The bundle is copied byte for byte, so its Developer ID signature stays valid.
"""
import argparse
import hashlib
import io
import json
import tarfile
import urllib.request
import zipfile
from pathlib import Path

root = Path(__file__).resolve().parent.parent
lock = json.loads((root / 'runtime/computer-use/manifest.json').read_text())


def digest(data):
    return hashlib.sha256(data).hexdigest()


def fetch(url):
    with urllib.request.urlopen(url, timeout=300) as response:
        return response.read()


def release(asset, sha):
    data = fetch(f"{lock['repository']}/releases/download/{lock['tag']}/{asset}")
    if digest(data) != sha:
        raise SystemExit(f'Computer Use archive SHA-256 mismatch: {asset}')
    return data


def windows():
    dest = root / 'runtime/computer-use/windows-x64'
    dest.mkdir(parents=True, exist_ok=True)
    binary = dest / 'cua-driver.exe'
    if not binary.exists() or digest(binary.read_bytes()) != lock['binarySha256']:
        with zipfile.ZipFile(io.BytesIO(release(lock['asset'], lock['archiveSha256']))) as files:
            data = files.read('cua-driver.exe')
        if digest(data) != lock['binarySha256']:
            raise SystemExit('Computer Use executable SHA-256 mismatch')
        binary.write_bytes(data)
    license_file = dest / 'LICENSE.md'
    if not license_file.exists() or digest(license_file.read_bytes()) != lock['licenseSha256']:
        data = fetch(f"https://raw.githubusercontent.com/trycua/cua/{lock['tag']}/LICENSE.md")
        if digest(data) != lock['licenseSha256']:
            raise SystemExit('Computer Use license SHA-256 mismatch')
        license_file.write_bytes(data)
    print(f"Prepared MIT cua-driver {lock['version']} (Windows x64), executable and license verified")


def macos(out):
    mac = lock['macos']
    source = tarfile.open(fileobj=io.BytesIO(release(mac['asset'], mac['archiveSha256'])), mode='r:gz')
    prefix = mac['asset'].removesuffix('.tar.gz') + '/'
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    seen = {}
    with tarfile.open(out, 'w:gz') as target:
        for member in source.getmembers():
            if not member.name.startswith(prefix):
                continue
            name = member.name[len(prefix):].rstrip('/')
            if not (name == 'LICENSE' or name == 'CuaDriver.app' or name.startswith('CuaDriver.app/')):
                continue
            if not (member.isfile() or member.isdir()) or '..' in name.split('/'):
                raise SystemExit(f'Unexpected entry in the driver archive: {member.name}')
            data = source.extractfile(member).read() if member.isfile() else None
            if data is not None:
                seen[name] = digest(data)
            member.name = name
            member.uid = member.gid = 0
            member.uname = member.gname = ''
            target.addfile(member, io.BytesIO(data) if data is not None else None)
    if seen.get('CuaDriver.app/Contents/MacOS/cua-driver') != mac['appBinarySha256']:
        raise SystemExit('CuaDriver.app executable SHA-256 mismatch')
    if seen.get('LICENSE') != lock['licenseSha256']:
        raise SystemExit('Computer Use license SHA-256 mismatch')
    print(f"Prepared MIT CuaDriver.app {lock['version']} (macOS universal) at {out}, executable and license verified")


parser = argparse.ArgumentParser()
parser.add_argument('--macos', metavar='OUT', help='write CuaDriver.app and its license to this .tgz')
args = parser.parse_args()
if args.macos:
    macos(args.macos)
else:
    windows()
