# bandit-X

Chat dari terminal (command line) dalam bahasa Indonesia.
Tanpa aplikasi tambahan, tanpa harus buka browser — semuanya dari layar hitam terminal.

Ada **2 cara pakai**:
1. **Chat biasa (server + client)** — seperti grup: ada satu "tuan rumah" yang buka pintunya, orang lain masuk.
2. **Chat Peer-to-Peer** — tanpa tuan rumah/server pusat. Semua orang saling terhubung langsung.

---

## Gampangnya: cara paling cepat mencoba

Ikuti langkah-langkah ini di laptop/PC. Yang perlu dibuka: aplikasi **Terminal**.

Kalau belum punya aplikasi ini, jalan pintasnya (Linux/macOS: Terminal, Windows: Windows Terminal).

### Langkah 1 — Buka 2 jendela terminal

Buka 2 jendela terminal di komputer yang sama. Jendela 1 jadi "tuan rumah", jendela 2 jadi "tamu".

### Langkah 2 — Jendela 1, jalankan server

```bash
bandit-x --server
```

Berarti: "buka ruangan chat di komputer ini". Biarkan dia jalan (jangan ditutup).

### Langkah 3 — Jendela 2, minta masuk sebagai tamu

```bash
bandit-x --client --username andi
```

Berarti: "aku si Andi, mau masuk ke ruangan itu". Nama bebas diganti sesukamu.

Muncul tampilan chat. Sekarang ketik apa saja terus tekan **Enter** — pesanmu muncul juga di jendela 1.

---

## Dua mode, dua arti

### Mode 1: Server & Client ("punya ruangan")

Cocok untuk: ngobrol ramai, satu orang jadi tuan rumah, yang lain bergabung ke ruangan itu.

- **Server** = pemilik ruangan. Menjalankan: `bandit-x --server`
- **Client** = orang yang masuk. Menjalankan: `bandit-x --client --username namamu`

Jika kamu menjalankan server dari komputer lain (misalnya VPS/sewaan internet),
temannya cukup menyebut alamatnya:

```bash
bandit-x --client --addr 123.123.123.123:8080 --username budi
```

Keterangan: `123.123.123.123` itu contoh alamat (IP) komputer server, `8080` nomor pintunya (port).

### Mode 2: Peer-to-Peer ("tanpa tuan rumah")

Cocok untuk: teman-teman yang saling kenal, tanpa perlu server pusat.
Semua orang hanya perlu tahu minimal 1 teman yang "bisa dihubungi".

```bash
# Node pertama (yang bisa dijangkau orang lain)
bandit-x --peer --listen 0.0.0.0:9090 --username alice

# Node berikutnya dari komputer lain — cukup sebut alamat alice
bandit-x --peer --listen 0.0.0.0:9091 --username bob --connect 123.123.123.123:9090
```

Catatan penting untuk mode 2:
- Tiap node wajib punya `--listen` yang **berbeda** (pintu berbeda).
- Yang `--connect` adalah orang yang masuk; boleh disebut lebih dari satu (pisahkan dengan koma).
- Syarat biar bisa ketemu di internet: minimal ada satu orang yang terminalnya bisa dijangkau dari luar (IP publik / port forwarding / VPS). Kalau masih di rumah/WiFi sendiri, pakailah alamat komputer teman di WiFi yang sama.

---

## Perintah di dalam chat

| Ketik | Arti |
|-------|------|
| `halo semua` | Mengirim pesan biasa ke semua orang |
| `/name Santi` | Ganti namamu |
| `/msg @budi apa kabar` | Kirim pesan khusus (private) ke Budi |
| `/quit` atau `/exit` | Keluar dari chat dengan sopan |

Contoh:

```
Kamu   : halo semua, ada orang?
Budi   : ada, aku Budi
Kamu   : /msg @budi coba kita diskusi privat
```

(Tanda `@` wajib sebelum nama saat kirim pesan privat.)

---

## Tombol yang perlu diketahui

| Tombol | Artinya |
|--------|---------|
| **Enter** | Kirim pesan |
| **Esc** | Keluar (tutup) |
| **PageUp / PageDown** | Lihat chat lama ke atas / ke bawah (10 baris) |
| **Alt + panah atas / bawah** | Geser chat naik / turun 1 baris |
| **Scroll mouse** | Geser chat |

---

## Cara dapat aplikasinya

### Cara A — Sudah ada instalannya (paling mudah)

Unduh dari tab **Releases** di GitHub repo ini, lalu jalankan `bandit-x`.

### Cara B — Pake Rust (untuk yang suka coding)

```bash
cargo install --git https://github.com/joyboyid/chatrtc.git
```

### Cara C — Dari kode ini sendiri (saat pengembangan)

```bash
cargo run -- --server
cargo run -- --client --username namaku
```

Selalu pakai `--server` atau `--client` atau `--peer`.
Aplikasi tidak akan jalan kalau ketiganya tidak dipilih.

---

## Kalau ada masalah (beginner-friendly)

| Gejala | Penyebabnya biasa | Solusinya |
|--------|-------------------|-----------|
| "command not found: bandit-x" | Program belum terpasang di PATH | Pasang lewat Releases atau instalasi Cara B/C |
| Client menunggu terus, tidak connect | Server tidak jalan / alamat salah / firewall | Pastikan server jalan (`--server`), cek alamat & port, buka port di firewall |
| Tampilan aneh di Windows | Pakai cmd.exe lama | Ganti ke **Windows Terminal** |
| Teman jauh tidak bisa masuk | Router/ISP memblokir | Buka port atau pakai VPS (mode server) |

---

## Penting diketahui

- Saat ini pesan **belum dienkripsi** (teks polos lewat koneksi). Cocok untuk kawan dan komunitas kecil, **belum untuk data rahasia**.
- Siapa pun yang tahu alamat & port bisa masuk. Belum ada kata sandi.

---

## Struktur proyek (untuk yang penasaran)

```
src/
├── main.rs     Titik masuk program (memilih server / client / peer)
├── message.rs  Aturan protokol pesan
├── server.rs   Mode chat dengan server
├── client.rs   Mode client (masuk ke server)
├── ui.rs       Tampilan di terminal
└── p2p.rs      Mode chat peer-to-peer (tanpa server)
```

Dokumen teknis lain: `note.txt` (tips pengembang), `analisis.txt` (analisis mode P2P), `develop.txt` (cara deploy ke internet), `distribusi.txt` (cara menyebar aplikasi ke banyak sistem).