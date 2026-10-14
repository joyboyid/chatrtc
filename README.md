# bandit-X

Chat dari terminal. Tanpa aplikasi tambahan, tanpa browser — semua dari layar hitam.

---

# CARA PAKAI

Pilih salah satu sesuai situasimu.

## A. Coba dulu di komputer sendiri

Buka **2 jendela terminal** di komputer yang sama.

**Jendela 1** — buka ruangannya:
```bash
bandit-x --server
```

**Jendela 2** — masuk ke ruangan itu:
```bash
bandit-x --client --username andi
```

Selesai. Ketik apa saja, tekan **Enter**.

---

## B. Server di VPS sendiri (punya IP publik)

Jalankan di VPS:
```bash
bandit-x --server --port 8080 --host 0.0.0.0
```

Buka portnya:
```bash
sudo ufw allow 8080/tcp
```

Teman cukup panggil alamat VPS-nya:
```bash
bandit-x --client --addr IP_VPS:8080 --username budi
```

Contoh: `--addr 123.45.67.89:8080`

---

## C. Hosting / Cloudflare (tidak punya IP publik)

Kalau domainmu sudah disambungkan ke Cloudflare, cara B **tidak akan nyambung** —
Cloudflare hanya meneruskan HTTP, sedangkan bandit-X bicara TCP biasa.
Solusinya: tambah satu flag `--ws`. Flag itu hanya mengubah cara kirim data,
isi chat tetap sama.

**1. Di VPS**, jalankan server (dengarkan di localhost saja, tidak perlu buka firewall):
```bash
bandit-x --server --port 8080 --host 127.0.0.1
```

**2. Di nginx**, arahkan ke server itu. Tambah di dalam blok `server { ... }`:
```nginx
location /chat {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade    $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host       $host;
    proxy_read_timeout  3600s;
    proxy_send_timeout  3600s;
}
```

Lalu `sudo nginx -t && sudo systemctl reload nginx`

> Baris `Upgrade` + `Connection` itu wajib. Tanpa itu, tidak akan tersambung.

**3. Di Cloudflare**, tidak perlu ubah apa-apa — WebSocket sudah nyala bawaan.
Pastikan SSL/TLS-nya bukan "Flexible".

**4. Teman/client** tinggal pakai:
```bash
bandit-x --client --ws --addr wss://orojackson.xcodehoster.id/chat --username budi
```

Ganti domain dan `/chat` dengan milikmu sendiri.

---

# PERINTAH DALAM CHAT

| Ketik | Arti |
|-------|------|
| `halo semua` | Kirim pesan ke semua orang |
| `/name Santi` | Ganti namamu |
| `/msg @budi apa kabar` | Kirim pesan private ke Budi (wajib ada `@`) |
| `/quit` atau `/exit` | Keluar |

---

# TOMBOL

| Tombol | Artinya |
|--------|---------|
| **Enter** | Kirim pesan |
| **Esc** | Keluar |
| **PageUp / PageDown** | Lihat chat lama |
| **Alt + panah atas/bawah** | Geser chat 1 baris |
| **Scroll mouse** | Geser chat |

---

# CARA DAPAT APLIKASINYA

**Paling mudah** — unduh dari tab **Releases** di GitHub repo ini.

**Pake Rust**:
```bash
cargo install --git https://github.com/joyboyid/chatrtc.git
```

**Dari kode ini**:
```bash
cargo run -- --server
cargo run -- --client --username namaku
```

Selalu pilih salah satu: `--server`, `--client`, atau `--ws`.

---

# KALAU ADA MASALAH

| Gejala | Penyebab | Solusi |
|--------|----------|--------|
| "command not found: bandit-x" | Belum terpasang | Pakai Releases atau `cargo install` |
| Nunggu terus, tidak connect | Server tidak jalan / alamat salah | Cek server jalan (`--server`) dan cek `--addr` |
| "is a websocket address — add --ws" | Lupa flag `--ws` | Tambahkan `--ws` (kasus C) |
| Tampilan aneh di Windows | Pakai cmd.exe lama | Ganti ke **Windows Terminal** |
| Teman jauh tidak bisa masuk | Router/ISP memblokir | Buka port, atau pakai hosting (kasus C) |

---

# PENTING DIKETAHUI

- Pesan **belum dienkripsi**. Cocok untuk kawan, **jangan untuk data rahasia**.
- **Tidak ada kata sandi.** Siapa pun yang tahu alamatmu bisa masuk.

---

# MODE LAIN: PEER-TO-PEER

Tanpa server — semua orang saling terhubung langsung.

```bash
# Node pertama
bandit-x --peer --listen 0.0.0.0:9090 --username alice

# Node berikutnya, cukup sebut alamat alice
bandit-x --peer --listen 0.0.0.0:9091 --username bob --connect 123.45.67.89:9090
```

- Tiap node harus punya `--listen` yang **berbeda**.
- `--connect` boleh lebih dari satu, pisahkan dengan koma.
- Tetap butuh minimal satu komputer yang bisa dihubungi dari luar.

---

# STRUKTUR PROYEK

```
src/
├── main.rs     Titik masuk (pilih server / client / peer)
├── message.rs  Aturan protokol pesan
├── server.rs   Mode chat dengan server
├── client.rs   Mode client (masuk ke server)
├── ws.rs       Lapisan WebSocket
├── ui.rs       Tampilan di terminal
└── p2p.rs      Mode peer-to-peer
```

Dokumen lain: `note.txt` (tips pengembang), `develop.txt` (deploy lengkap),
`analisis.txt` (analisis P2P), `distribusi.txt` (penyebaran aplikasi).