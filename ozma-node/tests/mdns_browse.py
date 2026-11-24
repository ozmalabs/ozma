#!/usr/bin/env python3
"""Browse for one exact `_ozma._udp.local.` instance and print it as JSON.

Used by `tests/mdns_live.rs`. Requires python-zeroconf, which is the same
library the controller's `DiscoveryService` uses.
"""

import json
import sys
import time

from zeroconf import ServiceBrowser, ServiceStateChange, Zeroconf


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: mdns_browse.py <instance> [timeout-seconds]", file=sys.stderr)
        return 2

    instance = sys.argv[1]
    timeout = float(sys.argv[2]) if len(sys.argv) > 2 else 8.0
    if not instance.endswith("."):
        instance += "."
    wanted = instance
    service_type = "_ozma._udp.local."
    result = {}

    zc = Zeroconf()

    def on_change(zeroconf, service_type, name, state_change):
        if name != wanted:
            return
        if state_change not in (ServiceStateChange.Added, ServiceStateChange.Updated):
            return
        info = zeroconf.get_service_info(service_type, name, timeout=3000)
        if info is None:
            return
        result.update(
            {
                "found": True,
                "type": info.type,
                "name": info.name,
                "server": info.server,
                "port": info.port,
                "addresses": info.parsed_addresses(),
                "host_ttl": info.host_ttl,
                "other_ttl": info.other_ttl,
                "properties": {
                    k.decode("utf-8", "replace"): (
                        v.decode("utf-8", "replace") if v is not None else None
                    )
                    for k, v in info.properties.items()
                },
            }
        )

    browser = ServiceBrowser(zc, service_type, handlers=[on_change])
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and not result:
        time.sleep(0.1)
    browser.cancel()
    zc.close()

    if not result:
        result = {"found": False, "name": wanted}
    print(json.dumps(result))
    return 0 if result.get("found") else 1


if __name__ == "__main__":
    sys.exit(main())
