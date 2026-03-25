# devnet-eez — Second Independent Devnet

Independent devnet for `your-dev-domain`. Runs simultaneously with testnet-eez on the same host using a +2000 port offset and distinct chain IDs (L1: 1338, L2: 42070 vs testnet-eez's L1: 1337, L2: 42069).

## Port Mapping

| Host Port | Service | Purpose |
|-----------|---------|---------|
| 11555 | l1 | L1 JSON-RPC |
| 11545 | builder | L2 JSON-RPC |
| 11546 | fullnode1 | L2 JSON-RPC |
| 11547 | fullnode2 | L2 JSON-RPC |
| 11548 | builder | L2 RPC Proxy |
| 11550 | builder | L2 WebSocket (localhost only) |
| 11556 | builder | L1 RPC Proxy |
| 11560 | builder | Health endpoint |
| 8081 | sync-ui | Dashboard |
| 5000 | l1-frontend | L1 Explorer UI |
| 5001 | l2-frontend | L2 Explorer UI |
| 5002 | l1-explorer | L1 Explorer API |
| 5003 | l2-explorer | L2 Explorer API |

## Quick Start

```bash
# Build once
cargo build --release

# Start (dev mode — mount local binary)
sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
                    -f deployments/devnet-eez/docker-compose.dev.yml up -d

# Iterate (after code changes)
cargo build --release
sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
                    -f deployments/devnet-eez/docker-compose.dev.yml \
                    restart builder fullnode1 fullnode2

# With explorers
sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
                    -f deployments/devnet-eez/docker-compose.dev.yml \
                    -f deployments/shared/docker-compose.explorer.yml \
                    -f deployments/devnet-eez/docker-compose.explorer.yml up -d

# Tear down
sudo docker compose -f deployments/devnet-eez/docker-compose.yml \
                    -f deployments/devnet-eez/docker-compose.dev.yml down
```

## Domain Setup (your-dev-domain)

### 1. DNS

Create A records pointing to this machine's public IP:

```
l1.your-dev-domain  → <IP>
l2.your-dev-domain  → <IP>
ui.your-dev-domain  → <IP>
```

### 2. TLS Certificates

```bash
sudo certbot certonly --nginx \
  -d l1.your-dev-domain \
  -d l2.your-dev-domain \
  -d ui.your-dev-domain
```

### 3. nginx (optional)

If you need reverse proxy access, create an nginx config manually. Example:
```bash
# Create /etc/nginx/sites-enabled/devnet-eez with proxy_pass to localhost:11545, etc.
sudo nginx -t && sudo systemctl reload nginx
```

## Shared Scripts

Deploy scripts are **not duplicated** — the deploy service mounts `../testnet-eez/deploy.sh` directly. All other scripts come from `../../scripts/`.

## Simultaneous Operation

Both devnets use isolated Docker Compose projects (separate networks, volumes, containers). No port conflicts:

| Resource | testnet-eez | devnet-eez |
|----------|-------------|------------|
| L1 RPC | 9555 | 11555 |
| L2 RPC | 9545 | 11545 |
| Proxy | 9548 | 11548 |
| Health | 9560 | 11560 |
| UI | 8080 | 8081 |
| L1 Explorer (UI/API) | 4000 / 4002 | 5000 / 5002 |
| L2 Explorer (UI/API) | 4001 / 4003 | 5001 / 5003 |
