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
   HDD busy with syncs), `api.listen`, and at least one `api.tokens` entry. A token is
   at least 32 visible ASCII characters; the daemon rejects shorter tokens and the
   example placeholder. Generate one with:

   ```sh
   openssl rand -hex 16
   ```

   `api.allow_unauthenticated = true` replaces the tokens only for a development
   listener on a loopback address. The file holds camera credentials and API tokens, so
   keep it readable by the service account only; the daemon warns at startup when users
   other than its account and group can read it:

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

5. Allow the camera network. The unit permits only the loopback
   (`IPAddressDeny=any`, `IPAddressAllow=localhost`), so the daemon reaches no camera
   until a drop-in adds the camera subnet. Add the API clients' subnet too when
   `api.listen` is a LAN address, and the DNS server's address when cameras are named by
   host name and the resolver is not on the loopback. Example for a documentation range:

   ```sh
   sudo install -d -m 755 /etc/systemd/system/irisd.service.d
   printf '[Service]\nIPAddressAllow=192.0.2.0/24\n' |
     sudo tee /etc/systemd/system/irisd.service.d/network.conf
   ```

   Allow lists from drop-ins are merged with the unit's own.

6. Install and start the unit:

   ```sh
   sudo install -Dm644 packaging/systemd/irisd.service /etc/systemd/system/irisd.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now irisd
   sudo systemctl status irisd
   ```

`systemd-analyze security irisd` reports the hardening score. The daemon pings the
watchdog only while every camera stream makes progress, so a wedged daemon is restarted
within `WatchdogSec`. Logs go to the journal: `journalctl -u irisd -f`.
`SyslogLevelPrefix=no` stores every line at the priority the daemon logged it with, so
camera-controlled text cannot raise a line's journal priority; irisd writes no priority
prefixes of its own.

The unit deliberately leaves `RestrictSUIDSGID` disabled. That systemd filter rejects
`openat2`, which the daemon requires for no-follow, beneath-root path resolution.
`NoNewPrivileges`, the empty capability set, `ProtectSystem=strict`, and Landlock retain the
service's privilege and filesystem boundaries.

The daemon also applies a Landlock sandbox on top of the unit's restrictions: it can read
its configuration directory, `/etc`, and `/proc`, read and write the recording roots and the
state directory, connect to the TCP ports the camera URLs name (554 when unspecified) and
each camera's `http_port`, and bind the API port. Landlock ABI 4 (Linux 6.7) or later is
required; on an older kernel, or one with Landlock disabled, the daemon refuses to start
unless `irisd run` is given `--no-sandbox`, which runs it unsandboxed with a warning.
Landlock filters ports for any destination address; the unit's `IPAddressAllow=` list
restricts the peers.

`StateDirectoryMode=0700` keeps the catalog directory private to the service account.
Before it opens the catalog, the daemon also requires the state directory and the
`irisd.db`, `irisd.db-wal`, and `irisd.db-shm` files to belong to the service account,
refuses symbolic or hard links and special files in their place, and removes group and
other permissions with a warning. An installation whose catalog was readable by other
users is therefore repaired at its next start; reading the catalog afterwards requires the
service account.

`LimitNOFILE=65536` gives recording files, camera sockets, and API clients one descriptor
budget; the daemon raises its soft limit to the hard limit at startup and logs both.

Upgrades follow the alpha policy in `STANDARDS.md` §9: configuration has no compatibility
promise before 1.0. A configuration with a token shorter than 32 characters, the example
placeholder, or an empty token list without `api.allow_unauthenticated` no longer loads;
generate a token before restarting the upgraded daemon. Storage settings outside the
documented ranges (`config/cameras.example.toml`) are rejected the same way.
