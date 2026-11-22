#!/usr/bin/env python3
"""Collect directly connected camera LAN identities; never scan routed networks."""
import concurrent.futures, ipaddress, json, os, socket, subprocess, time
from pathlib import Path
def collect(subnet="192.168.88", destination="/opt/camerahome-data/lan-neighbors.json"):
    network=ipaddress.IPv4Network(subnet+".0/24")
    if not network.is_private: raise ValueError("Private camera LAN required")
    route=json.loads(subprocess.check_output(["ip","-j","route","get",str(network.network_address+1)],timeout=5))[0]
    if route.get("gateway") or not route.get("dev"): raise ValueError("Camera LAN must be directly connected")
    def probe(ip):
        try:
            with socket.create_connection((str(ip),554),timeout=.6): return str(ip)
        except OSError: return None
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        active=set(filter(None,pool.map(probe,network.hosts())))
    neighbors={}
    for item in json.loads(subprocess.check_output(["ip","-j","neigh","show","dev",route["dev"]],timeout=5)):
        if item.get("dst") in active and item.get("lladdr"):
            neighbors[item["dst"]]=item["lladdr"].lower()
    path=Path(destination)
    temp=path.with_suffix(".new")
    temp.write_text(json.dumps({"at":int(time.time()),"neighbors":neighbors}))
    os.chmod(temp,0o600)
    os.replace(temp,path)
    print("LAN discovery: %s RTSP device(s)"%len(neighbors))
if __name__=="__main__":
    collect(os.environ.get("CAMERA_SUBNET","192.168.88"),os.environ.get("LAN_SNAPSHOT","/opt/camerahome-data/lan-neighbors.json"))
