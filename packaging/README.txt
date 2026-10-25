KVMHelm native package

Unpack into a new version directory. Verify the archive's external SHA-256,
then run verify.ps1 on Windows or ./verify.sh on Linux/macOS. SHA256SUMS detects
changed package files; it is not a signature or an independent trust source.
package.json records the exact inputs, target, inventory and supplied helpers.

Profiles: core contains the native daemon and embedded Vue assets; media adds
the FFmpeg helper for JetKVM video; full also adds the optional Node runtime
for existing trusted plugins. Other hardware/video/audio paths need no Node.
FFmpeg and Node are found beside the daemon or through KVMHELM_FFMPEG and
KVMHELM_NODE, then PATH. Full does not install npm or plugin dependencies.
The core never starts a Node process unless an enabled plugin requires it.

First use, with explicit private storage paths:
  kvmhelm token bootstrap --data DATA_DIRECTORY --secrets SECRET_DIRECTORY
  kvmhelm serve --data DATA_DIRECTORY --secrets SECRET_DIRECTORY
Open http://127.0.0.1:8765 and use the private SECRET_DIRECTORY/cli.pat file.
No demo devices are created by installation. Existing compatible data and
tokens need no second bootstrap. Back up data and secrets/master.key together.

Foreground: Ctrl+C stops accepted work, listeners, devices and owned helpers.
Linux/systemd: kvmhelm.service is an operator-installed template. Create its
service account and state directory first; bootstrap under that same account.
macOS/launchd: replace __PACKAGE_ROOT__ and __HOME__ with absolute XML-escaped
paths in local.kvmhelm.plist; load as a LaunchAgent for the data-owning user.
Windows/SCM: kvmhelm.exe service run accepts the same options as serve. SCM
registration and account selection are manual. Use the data-owning account;
LocalSystem cannot read another user's protected credentials by default.
SCM stop/shutdown uses the same graceful Core cleanup as foreground stop.
The package does not register a service or alter firewall rules on extraction.

For an update/rollback: stop the daemon/service, back up protected data/secrets,
verify a new sibling version directory and change the service's executable
path while stopped. Keep state outside package directories. Start and inspect
healthcheck/doctor. Preserve the old version and backup for a stopped rollback;
never run two daemon versions against the same data at once. Live sessions and
control leases do not survive restart, and input is never automatically replayed.

LAN requires explicit TLS certificate/key and allowed origins. healthcheck
reads /readyz without a token; --ca/KVMHELM_CA adds explicit CLI trust while
retaining hostname verification. KVMHELM_* settings must be supplied through
the process/service environment or flags; the native CLI does not load .env.
See docs/rust-port.md, docs/lan-access.md and docs/native-packaging.md in source.

licenses/ and THIRD-PARTY.json contain copied upstream notices and provenance,
including the helper-specific FFmpeg README/license and complete Node LICENSE.
The repository currently declares no application license. This package does
not choose or grant an additional application license and publishes nothing.
Build/runtime checks and platform/hardware limits are recorded separately;
packaging a target is not a claim of hardware or service acceptance.
