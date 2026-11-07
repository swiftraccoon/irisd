# Installing irisd as a service

1. Build the release binary and install it:

   ```sh
   cargo build --release -p irisd
   sudo install -Dm755 target/release/irisd /usr/local/bin/irisd
   ```

2. Create the service account and state directory:

   ```sh
   sudo install -Dm644 packaging/sysusers.d/irisd.conf /usr/lib/sysusers.d/irisd.conf
   sudo systemd-sysusers
   ```

3. Write the configuration. Start from `config/cameras.example.toml`; set
   `storage.video_roots` to the recording directory, `storage.state_dir` to
   `/var/lib/irisd` (the unit's `StateDirectory`, on the system disk: the catalog's
   transactions and queries take milliseconds there and up to a second on a recording
   HDD busy with syncs), `api.listen`, and at least one `api.tokens` entry. The file
   holds camera credentials, so keep it readable by the service account only:

   ```sh
   sudo install -d -m 750 -o root -g irisd /etc/irisd
   sudo install -m 640 -o root -g irisd my-irisd.toml /etc/irisd/irisd.toml
   ```

4. Create the recording root and give it to the service account:

   ```sh
   sudo install -d -m 750 -o irisd -g irisd /srv/irisd/video
   ```

   If the root is elsewhere, change `ReadWritePaths=` in the unit to name that exact
   configured root. Add one entry for each configured root rather than granting a parent
   directory.

   Catalog schema 9 upgrades require an operator-installed `.irisd-root-id` for every
   pre-identity recording root. Verify the intended storage is mounted and follow the durable
   marker procedure in [`04-storage-format.md` §3](../docs/research/04-storage-format.md#3-file-layout)
   before starting the upgraded daemon.

5. Install and start the unit:

   ```sh
   sudo install -Dm644 packaging/systemd/irisd.service /etc/systemd/system/irisd.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now irisd
   sudo systemctl status irisd
   ```

`systemd-analyze security irisd` reports the hardening score. The daemon pings the
watchdog only while every camera stream makes progress, so a wedged daemon is restarted
within `WatchdogSec`. Logs go to the journal: `journalctl -u irisd -f`.

The unit deliberately leaves `RestrictSUIDSGID` disabled. That systemd filter rejects
`openat2`, which the daemon requires for no-follow, beneath-root path resolution.
`NoNewPrivileges`, the empty capability set, `ProtectSystem=strict`, and Landlock retain the
service's privilege and filesystem boundaries.

The daemon also applies a Landlock sandbox on top of the unit's restrictions: it can read
its configuration directory, `/etc`, and `/proc`, read and write the recording roots and the
state directory, connect to the TCP ports the camera URLs name (554 when unspecified) and each camera's `http_port`, and bind the API
port. Pass `--no-sandbox` to `irisd run` only for debugging on kernels without Landlock.
