# LAN deployment

The daemon can serve local HTTP and LAN HTTPS through the same Core, device connections and control leases:

```powershell
./scripts/start-lan.ps1 -LanHost 192.0.2.10 -Certificate /protected/server.pem -Key /protected/server-key.pem
```

Replace the documentation address with the host's LAN address. The certificate must cover that address or hostname and be trusted by each client. The launcher requires an existing certificate and key; it does not change firewall rules or install a service. Stop the existing daemon before using it.

The local bridge uses http://127.0.0.1:8765; LAN clients use https://HOST:8766/mcp. Both require authentication. Keep PATs out of URLs and private keys outside the repository. Add extra browser origins with the daemon's --origin option when needed.
