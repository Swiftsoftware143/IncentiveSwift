#!/bin/bash
set -e
cd /opt/swift/incentiveswift
for i in $(seq 1 60); do
  if mkdir /tmp/rust-build.lock 2>/dev/null; then break; fi
  sleep 2
  if [ "$i" -eq 60 ]; then echo "ERROR: Could not acquire lock"; exit 1; fi
done
trap 'rmdir /tmp/rust-build.lock 2>/dev/null' EXIT
git pull origin main
systemctl restart incentiveswift
sleep 1
systemctl --no-pager status incentiveswift --no-pager | head -10
echo "=== Deploy complete ==="
