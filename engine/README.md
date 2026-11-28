# CameraHome native engine

Runs independently of Frigate: go2rtc restream, FFmpeg H.264/H.265 stream-copy, substream motion analysis, TFLite YAMNet baby-cry classification, and SQLite-backed independent R2/Drive copy queues. Rust remains the authenticated web and management API.

No public ports on go2rtc or engine. Only CameraHome binds 127.0.0.1:8180. The existing private camera network route is required. Do not publish RTSP credentials, go2rtc's internal API, R2 keys or Drive tokens.

## Capture

The primary stream is buffered into roughly 5-second TS segments; only segments overlapping motion or baby-cry events plus 8-second pre-roll and 12-second post-roll become archives. Static buffers are removed. Closed event clips are remuxed to MP4, keeping the original HD video codec and encoding only audio to AAC. Split points follow camera keyframes, with event batches capped at about 178 seconds. No AV1 CPU encoding. Disk free-space guard pauses new capture below 2 GiB; pending event footage is not silently erased.

Baby cry requires two consecutive YAMNet scores >=0.35 and uses the AudioSet baby-cry class, not generic sound volume. Motion uses a 320x180, 3fps substream, background subtraction, noise removal and consecutive-frame confirmation. These thresholds need validation in each room; this is not a safety-critical baby monitor. UI modes remain motion / crying / either. The legacy YAML structure is retained only as a migration format, not as a Frigate runtime dependency.

## Backup

Files are indexed only after atomic MP4 finalization. Each destination has its own durable state, retry time, target identity and verification. Two workers run independently; R2 starts first and becomes the primary playback source when enabled. Drive uses the local file if available, otherwise downloads a verified R2 original through the engine. R2-to-Drive is not a server-side copy. Upload checks size and MD5 if available; local and recovered originals are verified with SHA256. Never use S3 multipart ETags as MD5.

Retention never removes an unconfirmed local original. Local off stages are hidden from local playback and removed only after all active cloud copies confirm. R2 expiry waits for the active Drive copy. Offline/full/expired credentials preserve pending clips. Switches take effect during uploads within roughly a second. Admin sees safe queue counts and model health. Legacy cloud catalogs remain readable.

## Migration

Build and test the Rust web image first. Run `bash deploy/native-install.sh` on the existing host. It saves host-only migration backups and pins the pulled go2rtc digest in ignored deploy/.env. It reuses the already-installed YAMNet model from the cached upstream image; no Frigate service is started. Original archives and Frigate database are preserved. Existing backup switches are preserved. Configure R2 and Google Drive in the authenticated administration page before enabling those destinations.

The old clip timer is disabled. Existing per-camera cleanup and confirmed global purge remain available. The native global purge stops the engine before deleting local recordings and spool, cancels unfinished local backup jobs, and preserves confirmed cloud-copy indexes.

Rollback: stop native engine/go2rtc; restore the saved host compose/config and recreate the old services using their installed images; enable the old clips timer. No migration backup contains public repository files or credentials.

Tests: `python -m unittest discover -s tests -v`; inside engine image, `python -m unittest discover -s /tests -v`. Rust tests remain `cargo test --release`.
