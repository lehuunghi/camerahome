#!/bin/bash
set -euo pipefail
cd /opt/camerahome
backup_dir=/opt/camerahome-data/migration-$(date -u +%Y%m%dT%H%M%S)
mkdir -p "$backup_dir" /opt/camerahome-models /opt/camerahome-spool /opt/camerahome-data/native
chmod 700 "$backup_dir" /opt/camerahome-models /opt/camerahome-spool
cp /opt/frigate/config/config.yml "$backup_dir/config.yml"
cp /opt/camerahome/compose.yaml "$backup_dir/compose.yaml"
if [ -f /opt/camerahome-data/cloud-policy.json ]; then cp /opt/camerahome-data/cloud-policy.json "$backup_dir/cloud-policy.json"; fi
if [ ! -s /opt/camerahome-models/yamnet.tflite ]; then
  model_container=$(docker create ghcr.io/blakeblackshear/frigate:stable)
  docker cp "$model_container:/cpu_audio_model.tflite" /opt/camerahome-models/yamnet.tflite
  docker rm "$model_container"
fi
python3 - <<'PY'
from pathlib import Path
import urllib.request
path=Path('/opt/camerahome-models/yamnet_class_map.csv')
if not path.exists():
    with urllib.request.urlopen('https://raw.githubusercontent.com/tensorflow/models/master/research/audioset/yamnet/yamnet_class_map.csv', timeout=30) as r: raw=r.read()
    if b'/t/dd00002' not in raw or len(raw.splitlines())!=522: raise ValueError('invalid model labels')
    path.write_bytes(raw)
PY
docker pull alexxit/go2rtc:latest
printf 'GO2RTC_IMAGE=%s\n' "$(docker image inspect alexxit/go2rtc:latest --format '{{index .RepoDigests 0}}')" > deploy/.env
chmod 600 deploy/.env
docker compose -p camerahome -f deploy/native.compose.yaml build engine
docker compose -p camerahome -f deploy/native.compose.yaml run --rm --no-deps engine python service.py --prepare
systemctl disable --now camerahome-clips.timer
systemctl stop camerahome-clips.service
printf '%s\n' "$backup_dir" > /opt/camerahome-data/native-enabled
docker compose -p camerahome -f deploy/native.compose.yaml up -d --no-build
printf 'Migration backup: %s\n' "$backup_dir"
