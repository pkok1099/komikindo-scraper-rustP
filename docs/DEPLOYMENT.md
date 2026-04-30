# Deployment Guide

Panduan deploy komikindo-scraper ke berbagai platform: Termux (Android), Linux Server, dan GitHub Actions.

---

## Prerequisites

- **Rust toolchain** — `rustup` atau system package manager
- **CA certificates** — Untuk SSL/TLS verification
- **.env file** — Konfigurasi DATABASE_URL dan proxy

---

## Termux (Android ARM64)

### Install Rust

```bash
pkg update && pkg upgrade
pkg install rust git ca-certificates
```

### Build dari Source

```bash
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
# Binary: target/release/komikindo-scraper
```

**Build time:** ~5-15 menit tergantung device.

### Download Pre-built Binary

```bash
pkg install ca-certificates
wget -q https://github.com/pkok1099/komikindo-scraper-rust/releases/latest/download/komikindo-scraper-termux
chmod +x komikindo-scraper-termux
```

### Konfigurasi

```bash
# Buat .env file
echo "DATABASE_URL=postgresql://postgres.REF:PASS@aws-1-ap-southeast-1.pooler.supabase.com:6543/postgres" > .env

# Jika butuh proxy (opsional)
echo "PROXY_URL=socks5://127.0.0.1:1080" >> .env
echo "PROXY_ENABLED=1" >> .env
```

### CA Certificates

Termux memerlukan CA certificates untuk SSL:

```bash
pkg install ca-certificates
# Atau set manual:
export SSL_CERT_FILE=$PREFIX/etc/tls/cert.pem
```

### Run

```bash
# Test connectivity
./komikindo-scraper-termux debug

# Full fetch (concurrency rendah untuk Termux)
./komikindo-scraper-termux --max-in-flight 64 --max-blocking-threads 64 full-fetch

# Upload ke DB
./komikindo-scraper-termux upload-db

# Smart update
./komikindo-scraper-termux update --db
```

### Tips Termux

- **Wake lock** — Agar proses tidak terbunuh saat screen off:
  ```bash
  termux-wake-lock
  ```
- **Background process** — Gunakan `nohup` atau `tmux`:
  ```bash
  nohup ./komikindo-scraper-termux full-fetch > full_fetch.log 2>&1 &
  ```
- **Low memory** — Kurangi concurrency:
  ```bash
  ./komikindo-scraper-termux --max-in-flight 32 --max-blocking-threads 32 full-fetch --limit 100
  ```

---

## Linux Server

### Build dari Source

```bash
git clone https://github.com/pkok1099/komikindo-scraper-rust.git
cd komikindo-scraper-rust
cargo build --release
# Binary: target/release/komikindo-scraper
```

**Build time:** ~2-5 menit pada server modern.

### Install System Dependencies

```bash
# Ubuntu/Debian
sudo apt install ca-certificates

# CentOS/RHEL
sudo yum install ca-certificates
```

> **Note:** Binary di-compile dengan `static-curl` dan `rustls`, target MUSL (amd64) / static CRT (arm64). Tidak perlu system libcurl, OpenSSL, maupun glibc — binary berjalan langsung di distro Linux manapun.

### Konfigurasi

```bash
# Buat .env file
cat > .env << 'EOF'
DATABASE_URL=postgresql://postgres.REF:PASS@aws-1-ap-southeast-1.pooler.supabase.com:6543/postgres
PROXY_URL=socks5://127.0.0.1:1080
PROXY_ENABLED=1
SCRAPER_RETRIES=3
SCRAPER_TIMEOUT=30
EOF
```

### Run

```bash
# Test connectivity + DB
./target/release/komikindo-scraper debug

# Full fetch (high concurrency)
./target/release/komikindo-scraper --max-in-flight 512 --max-blocking-threads 512 full-fetch

# Upload ke DB
./target/release/komikindo-scraper upload-db

# Smart update
./target/release/komikindo-scraper update --db
```

### Systemd Service (Optional)

Untuk menjalankan smart update secara berkala:

```ini
# /etc/systemd/system/komikindo-update.service
[Unit]
Description=KomikIndo Smart Update
After=network.target

[Service]
Type=oneshot
User=scraper
WorkingDirectory=/opt/komikindo-scraper
ExecStart=/opt/komikindo-scraper/komikindo-scraper update --db
Environment=SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
```

```ini
# /etc/systemd/system/komikindo-update.timer
[Unit]
Description=Run KomikIndo Smart Update every 6 hours

[Timer]
OnCalendar=*-*-* 00,06,12,18:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable komikindo-update.timer
sudo systemctl start komikindo-update.timer
```

### Cron (Alternative)

```bash
# Edit crontab
crontab -e

# Run setiap 6 jam
0 */6 * * * cd /opt/komikindo-scraper && ./komikindo-scraper update --db >> /var/log/komikindo-update.log 2>&1
```

---

## GitHub Actions

### Smart Update (Auto Cron)

Workflow: `.github/workflows/update.yml`

Berjalan otomatis setiap 6 jam (07:00, 13:00, 19:00, 01:00 WIB).

**Setup:**
1. Tambahkan secret `DATABASE_URL` di:
   - Repo → Settings → Secrets and variables → Actions
   - New repository secret: `DATABASE_URL`
2. Workflow akan otomatis:
   - Build Rust binary (release, static curl + rustls)
   - Run `komikindo-scraper update --db`
   - Upsert results ke Supabase PostgreSQL

**Manual trigger:**
- Actions tab → Smart Update → Run workflow

**Limitations:**
- GitHub Actions IPs sering diblokir Cloudflare
- Jika gagal, coba gunakan proxy atau jalankan manual dari server/IP yang tidak diblokir

### Build Release

Workflow: `.github/workflows/release.yml`

Membuat binary untuk 2 platform:

| Target | Arch | Platform | Notes |
|--------|------|----------|-------|
| `x86_64-unknown-linux-musl` | x86_64 | Linux (Server/PC) | **SELF-CONTAINED** — static-pie linked, zero glibc dependency |
| `aarch64-linux-android` | aarch64 | Termux (Android) | **SELF-CONTAINED** — statically linked, zero dependency |

**Trigger:**
- Push tag `v*` (e.g., `git tag v1.2.0 && git push origin v1.2.0`)
- Manual dispatch dari Actions tab

**Output:**
- `komikindo-scraper-amd64` — Linux binary (self-contained, zero deps)
- `komikindo-scraper-termux` — Termux binary (self-contained, zero deps)

---

## Proxy Setup

Beberapa IP (terutama datacenter IPs) diblokir oleh Cloudflare. Gunakan proxy untuk bypass:

### SOCKS5 Proxy

```bash
# Via CLI flag
komikindo-scraper full-fetch --proxy socks5://127.0.0.1:1080

# Via .env
echo "PROXY_URL=socks5://127.0.0.1:1080" >> .env
echo "PROXY_ENABLED=1" >> .env
```

### Remote DNS Resolution

Jika DNS resolution gagal melalui proxy, gunakan `socks5h://` (remote DNS):

```bash
komikindo-scraper full-fetch --proxy socks5h://127.0.0.1:1080
```

### SSH Tunnel

```bash
# Setup SSH tunnel
ssh -D 1080 -N user@remote-server &

# Gunakan tunnel sebagai proxy
komikindo-scraper full-fetch --proxy socks5://127.0.0.1:1080
```

---

## Troubleshooting

### Cloudflare Challenge

**Gejala:** Response berisi "Just a moment..." atau "Checking your browser"

**Solusi:**
1. Gunakan proxy (residential IP lebih reliable)
2. Tidak ada cara programmatic untuk bypass Cloudflare challenge — ini adalah anti-bot protection
3. Verifikasi dengan `komikindo-scraper -v check`

### SSL/TLS Errors

**Gejala:** "SSL certificate problem" atau "CA bundle NOT FOUND"

**Solusi:**
```bash
# Termux
pkg install ca-certificates
export SSL_CERT_FILE=$PREFIX/etc/tls/cert.pem

# Linux
sudo apt install ca-certificates
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

# macOS
export SSL_CERT_FILE=/etc/ssl/cert.pem
```

### Database Connection Failed

**Gejala:** "Failed to connect to Supabase PostgreSQL"

**Solusi:**
1. Check DATABASE_URL format: `postgresql://user:pass@host:5432/db`
2. Gunakan port 5432 (bukan 6543 PgBouncer) — kode otomatis redirect
3. Check network connectivity ke DB host
4. Verify credentials benar
5. Test dengan `komikindo-scraper debug --db`

### OOM (Out of Memory)

**Gejala:** Process killed, dmesg shows "Out of memory"

**Solusi:**
```bash
# Kurangi concurrency
komikindo-scraper --max-in-flight 64 --max-blocking-threads 64 full-fetch

# Gunakan limit
komikindo-scraper full-fetch --limit 1000

# Gunakan resume untuk batch processing
komikindo-scraper full-fetch --limit 2000
# ... jika crash, resume:
komikindo-scraper full-fetch --resume
```

### Slow Performance

**Diagnosa:**
```bash
# Check connectivity speed
komikindo-scraper -v check

# Benchmark parsing
komikindo-scraper bench-parse --kind detail

# Check DB latency
komikindo-scraper debug --db
```

**Possible causes:**
1. Network latency — Gunakan proxy yang lebih dekat ke target
2. Rate limiting (429 errors) — Kurangi `--max-in-flight`
3. DB latency — Supabase round-trip ~100-300ms normal
4. DNS resolution — Set `SCRAPER_TIMEOUT` lebih tinggi
