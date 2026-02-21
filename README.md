# 🚀 Nitro Downloader Pro — v2.0

מנוע הורדות **מהיר, חכם ואמין** כתוב ב-Rust.  
תומך ב-multi-part parallel download, resume אוטומטי, ואימות SHA256.

---

## ✨ פיצ'רים

| פיצ'ר | פירוט |
|-------|-------|
| ⚡ Multi-part | 8 חיבורים במקביל — מקסימום מהירות |
| 🔄 Resume חכם | ממשיך מאיפה שנעצר — ETag matching |
| 🛡️ אימות SHA256 | וידוא שלמות הקובץ אחרי הורדה |
| 🔁 Auto-retry | עד 10 ניסיונות עם exponential backoff |
| 🤖 Anti-bot headers | עובד גם עם Cloudflare ושרתים מוגנים |
| 📊 Progress bar | מהירות + ETA בזמן אמת |
| 💾 Pre-allocation | מקצה מקום מראש — ללא פיצול קובץ |
| 🪟 Cross-platform | Linux + Windows |

---

## ⚡ בנייה

### דרישות
```bash
# Rust 1.75+
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Cargo.toml
```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["stream", "gzip", "brotli"] }
futures = "0.3"
indicatif = "0.17"
colored = "2"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
hex = "0.4"
chrono = { version = "0.4", features = ["serde"] }
```

### בנייה והתקנה
```bash
cargo build --release
sudo cp target/release/nitro /usr/local/bin/
```

---

## 🎛️ שימוש

### הפעלה
```bash
nitro
```

### זרימת עבודה
```
🔗 Enter URL (or 'exit'): https://example.com/ubuntu-24.04.iso

📡 Fetching file information...
📦 File size: 1024 MB
🔄 Range support: ✅ Yes
🚀 Starting 8 parallel streams

⠿ [00:01:23] [████████████████░░░░] 800MB/1024MB (12.3 MB/s, ETA: 18s)

✅ SUCCESS: File downloaded successfully!
📁 Location: ./downloads/ubuntu-24.04.iso

🔐 Verify file integrity? Enter SHA256 hash (or press Enter to skip):
```

### אימות SHA256
```bash
# אחרי הורדה — הכנס את ה-hash מהאתר הרשמי
🔐 Verify: a435f6f393dda581172490ead4ee5...

✅ SECURITY PASS: Hash matches!
```

---

## 🔧 איך זה עובד

### Multi-part Download
```
קובץ 1GB
├── Worker 0: bytes 0       → 128MB   ████████░░░░░░░░
├── Worker 1: bytes 128MB   → 256MB   ████████░░░░░░░░
├── Worker 2: bytes 256MB   → 384MB   ████████░░░░░░░░
├── Worker 3: bytes 384MB   → 512MB   ██████░░░░░░░░░░
├── Worker 4: bytes 512MB   → 640MB   █████████░░░░░░░
├── Worker 5: bytes 640MB   → 768MB   ███████░░░░░░░░░
├── Worker 6: bytes 768MB   → 896MB   ████████░░░░░░░░
└── Worker 7: bytes 896MB   → 1GB     ██████████░░░░░░
```

### Resume Logic
```
הורדה נקטעת בשביל X
       ↓
.meta file נשמר עם מצב כל chunk
       ↓
הרצה מחדש → ETag matching
       ↓
רק chunks שלא הסתיימו ממשיכים
       ↓
✅ ממשיך מאיפה שנעצר
```

### Anti-bot Strategy
```
User-Agent    → Chrome 121 real browser
sec-ch-ua     → Chrome fingerprint
Referer       → domain/  (auto-generated)
DNT           → 1
Keep-Alive    → 90 seconds
TCP Keepalive → 30 seconds
```

---

## 📁 קבצי מערכת

```
downloads/
├── ubuntu-24.04.iso        ← הקובץ
└── ubuntu-24.04.iso.meta   ← מצב ההורדה (נמחק אחרי השלמה)
```

### מבנה .meta
```json
{
  "url": "https://...",
  "etag": "\"abc123\"",
  "total_size": 1073741824,
  "downloaded": 536870912,
  "timestamp": 1708512000,
  "chunks_completed": [true, true, false, false, true, false, true, false]
}
```

---

## ⚙️ הגדרות

| פרמטר | ברירת מחדל | תיאור |
|-------|-----------|-------|
| `num_workers` | 8 | חיבורים במקביל |
| `buffer_size` | 8 MB | גודל באפר לכל worker |
| `pool_idle_timeout` | 90s | שמירת חיבור פתוח |
| `tcp_keepalive` | 30s | מניעת ניתוק |
| `timeout` | 300s | timeout כולל |
| `max_retries` | 10 | ניסיונות חוזרים |

---

## 🆘 פתרון בעיות

| בעיה | פתרון |
|------|-------|
| `HEAD blocked` | אוטומטי — עובר ל-GET עם Range |
| `403 Forbidden` | הוסף Referer ידנית בקוד |
| הורדה איטית | בדוק מהירות האינטרנט שלך |
| Resume לא עובד | בדוק שה-.meta קיים ולא נמחק |
| Hash mismatch | הורד שוב — קובץ פגום |
| SSL error | `insecure: true` (לא מומלץ) |

---

## 🗺️ Roadmap

- [ ] `clap` CLI — `nitro download <url> --workers 16 --output ~/Downloads`
- [ ] progress bar כפול — per-chunk + total
- [ ] throttle — הגבלת מהירות
- [ ] queue — רשימת הורדות
- [ ] egui / ICED wrapper — ממשק גרפי

---

## 📜 רישיון

MIT — קוד פתוח, לטובת הקהילה. 🦀
