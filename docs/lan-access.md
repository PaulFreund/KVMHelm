# LAN deployment

The daemon can serve local HTTP and LAN HTTPS through the same Core, device connections and control leases:

```powershell
./scripts/start-lan.ps1 -LanHost 192.0.2.10 -Certificate /protected/server.pem -Key /protected/server-key.pem
```

Replace the documentation address with the host's LAN address. The certificate must cover that address or hostname and be trusted by each client. The launcher requires an existing certificate and key; it does not change firewall rules or install a service. Stop the existing daemon before using it.

On the Rust port branch the launcher starts the native release executable at
`target/x86_64-pc-windows-msvc/release/kvmhelm.exe`. Build it first or pass
`-Binary` with an extracted package executable. `-Data` and `-Secrets` select
existing protected state; defaults are the repository's `.kvmhelm` directories.
The native process does not load `.env`. See
[native packages and migration](native-packaging.md) for helper profiles,
account permissions and stopped updates. `npm start` also starts Rust from
`target/release`; the TypeScript original is isolated test data only.

The local bridge uses http://127.0.0.1:8765; LAN clients use https://HOST:8766/mcp. Both require authentication. Keep PATs out of URLs and private keys outside the repository. Add extra browser origins with the daemon's --origin option when needed.
