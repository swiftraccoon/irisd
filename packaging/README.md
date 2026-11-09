# Installing irisd as a service

1. Build the release binary and install it:

   ```sh
   cargo build --release -p irisd
   sudo install -Dm755 target/release/irisd /usr/local/bin/irisd
   ```

2. Create the service account (the unit's `StateDirectory=` creates `/var/lib/irisd`, mode
   0700, at first start):

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

   A root the daemon cannot identify stays unavailable: one without a `.irisd-root-id` marker
   that holds files or catalog rows, and every unregistered root of a catalog created before
   schema 9. Verify the intended storage is mounted, then install a marker by the durable
   procedure in [`04-storage-format.md` §3](../docs/research/04-storage-format.md#3-file-layout).

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

   Allow lists from drop-ins are merged with the unit's own. With `[provisioning] enabled`,
   add `multicast` to the list (`IPAddressAllow=192.0.2.0/24 multicast`) so the viewer's camera
   discovery can send its WS-Discovery probe; without it the probe is dropped and discovery
   lists nothing.

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
`SyslogLevelPrefix=no` stores every line at the unit's `SyslogLevel=` (info by default), so
camera-controlled text cannot raise a line's journal priority. irisd writes no priority
prefixes: its level appears only as text in each line, so filter by that text
(`journalctl -u irisd --grep WARN`), not with `-p`.

The unit deliberately leaves `RestrictSUIDSGID` disabled. That systemd filter rejects
`openat2`, which the daemon requires for no-follow, beneath-root path resolution. Without it,
the daemon could mark a file it owns set-user-ID. Its writable paths are the state directory
(0700), the recording roots (0750, group `irisd`), and a private `/tmp` and `/var/tmp`, so
keep login users out of the `irisd` group and mount recording filesystems `nosuid` where
possible.

The daemon also applies a Landlock sandbox on top of the unit's restrictions: it can read
its configuration directory, `/etc`, and `/proc`; read and write the recording roots and the
state directory; connect to the TCP ports the camera URLs name (554 when unspecified), each
camera's `http_port`, and, with provisioning enabled, the `provisioning.connect_ports`; and
bind the API port. Landlock ABI 4 (Linux 6.7) or later is required; on an older kernel, or
one with Landlock disabled, the daemon refuses to start unless `irisd run` is given
`--no-sandbox`, which runs it unsandboxed with a warning. Landlock filters ports for any
destination address; the unit's `IPAddressAllow=` list restricts the peers.

`StateDirectoryMode=0700` keeps the catalog directory private to the service account. Before it
opens the catalog, the daemon also requires the state directory and the `irisd.db`, `irisd.db-wal`,
`irisd.db-shm`, `capability.key`, and `cameras.toml` files to belong to the service account, refuses
symbolic or hard links and special files in their place, and removes group and other permissions
with a warning. An installation whose catalog was readable by other users is therefore repaired at
its next start; reading the catalog afterwards requires the service account.

`LimitNOFILE=65536` gives recording files, camera sockets, and API clients one descriptor
budget; the daemon raises its soft limit to the hard limit at startup and logs both.

Upgrades follow the alpha policy in `STANDARDS.md` §9: configuration has no compatibility
promise before 1.0, and the daemon refuses to start on a configuration that breaks the token
rules of step 3 or the ranges in `config/cameras.example.toml`.
