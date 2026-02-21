use anyhow::{Context, Result, bail};
use colored::Colorize;
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::JoinSet;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

// ========== מבנה Metadata לשמירת מצב ההורדה ==========
#[derive(Serialize, Deserialize, Debug, Clone)]
struct DownloadMetadata {
    url: String,
    etag: Option<String>,
    total_size: u64,
    downloaded: u64,
    timestamp: i64,
    chunks_completed: Vec<bool>, // Track which chunks finished
}

impl DownloadMetadata {
    fn metadata_path(output: &Path) -> PathBuf {
        output.with_extension("meta")
    }

    fn save(&self, output: &Path) -> Result<()> {
        let meta_path = Self::metadata_path(output);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(meta_path, json)?;
        Ok(())
    }

    fn load(output: &Path) -> Result<Option<Self>> {
        let meta_path = Self::metadata_path(output);
        if !meta_path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(meta_path)?;
        let meta: DownloadMetadata = serde_json::from_str(&json)?;
        Ok(Some(meta))
    }

    fn delete(output: &Path) -> Result<()> {
        let meta_path = Self::metadata_path(output);
        if meta_path.exists() {
            std::fs::remove_file(meta_path)?;
        }
        Ok(())
    }
}

// ========== מנוע ההורדה המשופר ==========
#[derive(Clone)]
pub struct DownloadEngine {
    client: Client,
    mp: MultiProgress,
    num_workers: usize,
    buffer_size: usize,
}

impl DownloadEngine {
    pub fn new(insecure: bool, num_workers: Option<usize>) -> Self {
        let mut headers = header::HeaderMap::new();

        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36"
            )
        );

        // Headers נגד חסימות בוטים (Cloudflare וכו')
        headers.insert("sec-ch-ua", header::HeaderValue::from_static("\"Not A(Machine;Brand\";v=\"99\", \"Google Chrome\";v=\"121\", \"Chromium\";v=\"121\""));
        headers.insert("sec-ch-ua-mobile", header::HeaderValue::from_static("?0"));
        headers.insert(
            "sec-ch-ua-platform",
            header::HeaderValue::from_static("\"Windows\""),
        );

        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
            ),
        );
        headers.insert(
            header::ACCEPT_LANGUAGE,
            header::HeaderValue::from_static("en-US,en;q=0.9"),
        );
        headers.insert(
            header::ACCEPT_ENCODING,
            header::HeaderValue::from_static("gzip, deflate, br"),
        );
        headers.insert("DNT", header::HeaderValue::from_static("1"));
        headers.insert("Connection", header::HeaderValue::from_static("keep-alive"));
        headers.insert(
            "Upgrade-Insecure-Requests",
            header::HeaderValue::from_static("1"),
        );

        let mut builder = Client::builder()
            .default_headers(headers)
            .http1_only()
            .pool_max_idle_per_host(20)
            .pool_idle_timeout(std::time::Duration::from_secs(90)) // <-- הקסם שמונע ניתוק משרתים
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::limited(10));

        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
            println!(
                "{}",
                "⚠️  SECURITY WARNING: SSL verification disabled!"
                    .on_red()
                    .white()
                    .bold()
            );
        }

        let client = builder.build().expect("Failed to build HTTP client");

        DownloadEngine {
            client,
            mp: MultiProgress::new(),
            num_workers: num_workers.unwrap_or(8),
            buffer_size: 8 * 1024 * 1024,
        }
    }
    // ========== פונקציה ראשית להורדה ==========

    pub async fn download(&self, url: &str, output: impl AsRef<Path>) -> Result<()> {
        let path = output.as_ref();

        // יצירת תיקיית downloads אם לא קיימת
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // שלב 1: קבלת מידע על הקובץ מהשרת
        println!("{}", "📡 Fetching file information...".bright_cyan());

        // נוסיף Referer header בהתאם ל-URL
        let referer = if let Ok(parsed) = reqwest::Url::parse(url) {
            if let Some(domain) = parsed.domain() {
                format!("https://{}/", domain)
            } else {
                "https://www.google.com/".to_string()
            }
        } else {
            "https://www.google.com/".to_string()
        };

        let head_resp = self
            .client
            .head(url)
            .header(header::REFERER, &referer)
            .send()
            .await
            .context("Failed to send HEAD request")?;

        // אם HEAD החזיר 403, ננסה GET עם Range: bytes=0-0
        let (total_size, supports_range, server_etag) = if head_resp.status()
            == StatusCode::FORBIDDEN
        {
            println!(
                "{}",
                "⚠️  HEAD blocked, trying alternative method...".yellow()
            );

            let get_resp = self
                .client
                .get(url)
                .header(header::REFERER, &referer)
                .header(header::RANGE, "bytes=0-0")
                .send()
                .await
                .context("Failed to send GET request")?;

            if !get_resp.status().is_success() && get_resp.status() != StatusCode::PARTIAL_CONTENT {
                bail!("Server returned error: {}", get_resp.status());
            }

            let size = get_resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    // Content-Range: bytes 0-0/1234567
                    v.split('/').nth(1)?.parse::<u64>().ok()
                })
                .or_else(|| {
                    get_resp
                        .headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .context("Server did not provide file size")?;

            let range = get_resp
                .headers()
                .get(header::ACCEPT_RANGES)
                .map_or(true, |v| v == "bytes"); // אם תמך ב-Range request, כנראה שיש תמיכה

            let etag = get_resp
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            (Some(size), range, etag)
        } else if !head_resp.status().is_success() {
            bail!("Server returned error: {}", head_resp.status());
        } else {
            let size = head_resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .context("Server did not provide Content-Length")?;

            let range = head_resp
                .headers()
                .get(header::ACCEPT_RANGES)
                .map_or(false, |v| v == "bytes");

            let etag = head_resp
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            (Some(size), range, etag)
        };

        // Unwrap total_size or error if None
        let total_size = total_size.context("Could not determine file size")?;

        println!("📦 File size: {} MB", total_size / 1024 / 1024);
        println!(
            "🔄 Range support: {}",
            if supports_range { "✅ Yes" } else { "❌ No" }
        );
        if let Some(ref etag) = server_etag {
            println!("🏷️  ETag: {}", etag);
        }

        // שלב 2: בדיקת קובץ קיים ו-Resume Logic
        if path.exists() {
            if let Some(meta) = DownloadMetadata::load(path)? {
                // בדיקה אם זה אותו קובץ (ETag matching)
                if meta.etag != server_etag && server_etag.is_some() {
                    println!(
                        "{}",
                        "⚠️  Server file has changed (ETag mismatch). Restarting download."
                            .yellow()
                    );
                    std::fs::remove_file(path)?;
                    DownloadMetadata::delete(path)?;
                } else if meta.total_size != total_size {
                    println!(
                        "{}",
                        "⚠️  File size mismatch. Restarting download.".yellow()
                    );
                    std::fs::remove_file(path)?;
                    DownloadMetadata::delete(path)?;
                } else {
                    // אפשר לעשות Resume!
                    let current_len = path.metadata()?.len();
                    let progress = (current_len as f64 / total_size as f64 * 100.0) as u64;

                    if current_len == total_size {
                        println!("{}", "✅ File already complete!".green().bold());
                        DownloadMetadata::delete(path)?;
                        return Ok(());
                    }

                    if current_len < total_size && supports_range {
                        println!(
                            "🔄 Resuming from {} MB ({}% complete)",
                            current_len / 1024 / 1024,
                            progress
                        );

                        return self
                            .resume_download(url, path, meta, total_size, server_etag)
                            .await;
                    }
                }
            } else {
                // קובץ קיים בלי metadata - בודקים גודל
                let current_len = path.metadata()?.len();
                if current_len == total_size {
                    println!(
                        "{}",
                        "✅ File exists and matches expected size. Skipping.".green()
                    );
                    return Ok(());
                }
                println!(
                    "{}",
                    "⚠️  Existing file without metadata. Restarting.".yellow()
                );
                std::fs::remove_file(path)?;
            }
        }

        // שלב 3: התחלת הורדה חדשה
        if supports_range && total_size > 10 * 1024 * 1024 {
            // קבצים מעל 10MB - multi-part
            println!(
                "{}",
                format!("🚀 Starting {} parallel streams", self.num_workers)
                    .bright_cyan()
                    .bold()
            );
            self.run_multi_part(url, path, total_size, server_etag)
                .await
        } else {
            println!("{}", "📥 Starting single stream download".bright_cyan());
            self.run_single_stream(url, path, total_size, 0, server_etag)
                .await
        }
    }

    // ========== Resume הורדה קיימת ==========
    async fn resume_download(
        &self,
        url: &str,
        path: &Path,
        meta: DownloadMetadata,
        total_size: u64,
        server_etag: Option<String>,
    ) -> Result<()> {
        // אם יש chunks_completed, נשתמש בזה לחידוש multi-part
        if meta.chunks_completed.len() == self.num_workers {
            return self
                .resume_multi_part(url, path, meta, total_size, server_etag)
                .await;
        }

        // אחרת - single stream resume
        let current_len = path.metadata()?.len();
        self.run_single_stream(url, path, total_size, current_len, server_etag)
            .await
    }

    // ========== Multi-part Download (משופר עם error handling) ==========
    async fn run_multi_part(
        &self,
        url: &str,
        path: &Path,
        total_size: u64,
        server_etag: Option<String>,
    ) -> Result<()> {
        // יצירת metadata
        let mut meta = DownloadMetadata {
            url: url.to_string(),
            etag: server_etag,
            total_size,
            downloaded: 0,
            timestamp: chrono::Utc::now().timestamp(),
            chunks_completed: vec![false; self.num_workers],
        };

        // יצירת קובץ והקצאת מקום
        let std_file = File::create(path).context("Failed to create output file")?;
        std_file
            .set_len(total_size)
            .context("Failed to preallocate file space")?;
        let file_arc = Arc::new(std_file);

        // Progress bar
        let pb = Arc::new(self.mp.add(ProgressBar::new(total_size)));
        pb.set_style(
            ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta})")?
            .progress_chars("█▓▒░ ")
        );

        let downloaded = Arc::new(AtomicU64::new(0));
        let mut workers = JoinSet::new();
        let part_size = total_size / self.num_workers as u64;

        // פיצול העבודה ל-workers
        for i in 0..self.num_workers {
            let start = i as u64 * part_size;
            let end = if i == self.num_workers - 1 {
                total_size - 1
            } else {
                ((i + 1) as u64 * part_size) - 1
            };

            let client = self.client.clone();
            let url = url.to_string();
            let file_ref = Arc::clone(&file_arc);
            let downloaded_arc = Arc::clone(&downloaded);
            let pb_clone = Arc::clone(&pb);
            let buffer_size = self.buffer_size;

            workers.spawn(async move {
                Self::download_chunk(
                    client,
                    &url,
                    file_ref,
                    start,
                    end,
                    downloaded_arc,
                    pb_clone,
                    buffer_size,
                    i,
                )
                .await
            });
        }

        // המתנה לכל ה-workers
        let mut completed_chunks = vec![false; self.num_workers];
        while let Some(result) = workers.join_next().await {
            match result {
                Ok(Ok(chunk_id)) => {
                    completed_chunks[chunk_id] = true;
                    meta.chunks_completed = completed_chunks.clone();
                    meta.downloaded = downloaded.load(Ordering::Relaxed);
                    meta.save(path)?;
                }
                Ok(Err(e)) => {
                    pb.finish_with_message("Failed!");
                    return Err(e).context("Worker failed");
                }
                Err(e) => {
                    pb.finish_with_message("Failed!");
                    return Err(e.into());
                }
            }
        }

        // Sync final
        file_arc.sync_all().context("Failed to sync file")?;
        pb.finish_with_message("✅ Download complete!");

        // מחיקת metadata - ההורדה הושלמה
        DownloadMetadata::delete(path)?;

        Ok(())
    }

    // ========== Resume multi-part ==========
    async fn resume_multi_part(
        &self,
        url: &str,
        path: &Path,
        mut meta: DownloadMetadata,
        total_size: u64,
        _server_etag: Option<String>,
    ) -> Result<()> {
        let std_file = File::options()
            .write(true)
            .open(path)
            .context("Failed to open file for resume")?;
        let file_arc = Arc::new(std_file);

        let pb = Arc::new(self.mp.add(ProgressBar::new(total_size)));
        let current_downloaded = meta.downloaded;
        pb.set_position(current_downloaded);

        pb.set_style(
            ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta})")?
            .progress_chars("█▓▒░ ")
        );

        let downloaded = Arc::new(AtomicU64::new(current_downloaded));
        let mut workers = JoinSet::new();
        let part_size = total_size / self.num_workers as u64;

        // רק chunks שלא הסתיימו
        for i in 0..self.num_workers {
            if meta.chunks_completed[i] {
                continue; // Skip completed chunks
            }

            let start = i as u64 * part_size;
            let end = if i == self.num_workers - 1 {
                total_size - 1
            } else {
                ((i + 1) as u64 * part_size) - 1
            };

            let client = self.client.clone();
            let url = url.to_string();
            let file_ref = Arc::clone(&file_arc);
            let downloaded_arc = Arc::clone(&downloaded);
            let pb_clone = Arc::clone(&pb);
            let buffer_size = self.buffer_size;

            workers.spawn(async move {
                Self::download_chunk(
                    client,
                    &url,
                    file_ref,
                    start,
                    end,
                    downloaded_arc,
                    pb_clone,
                    buffer_size,
                    i,
                )
                .await
            });
        }

        let mut completed_chunks = meta.chunks_completed.clone();
        while let Some(result) = workers.join_next().await {
            match result {
                Ok(Ok(chunk_id)) => {
                    completed_chunks[chunk_id] = true;
                    meta.chunks_completed = completed_chunks.clone();
                    meta.downloaded = downloaded.load(Ordering::Relaxed);
                    meta.save(path)?;
                }
                Ok(Err(e)) => {
                    pb.finish_with_message("Failed!");
                    return Err(e).context("Worker failed during resume");
                }
                Err(e) => {
                    pb.finish_with_message("Failed!");
                    return Err(e.into());
                }
            }
        }

        file_arc.sync_all()?;
        pb.finish_with_message("✅ Download complete!");
        DownloadMetadata::delete(path)?;

        Ok(())
    }

    // ========== Worker function להורדת chunk בודד ==========
    async fn download_chunk(
        client: Client,
        url: &str,
        file: Arc<File>,
        start: u64,
        end: u64,
        downloaded: Arc<AtomicU64>,
        pb: Arc<ProgressBar>,
        buffer_size: usize,
        chunk_id: usize,
    ) -> Result<usize> {
        let mut current_pos = start;
        let mut buffer = Vec::with_capacity(buffer_size);
        const MAX_RETRIES: u32 = 10;
        let mut retries = 0;

        while current_pos <= end {
            if retries >= MAX_RETRIES {
                bail!("Chunk {} exceeded max retries", chunk_id);
            }

            let range = format!("bytes={}-{}", current_pos, end);
            let response = match client.get(url).header(header::RANGE, &range).send().await {
                Ok(_r)
                    if _r.status() == StatusCode::PARTIAL_CONTENT || _r.status().is_success() =>
                {
                    _r
                }
                Ok(_r) => {
                    retries += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(2_u64.pow(retries.min(5))))
                        .await;
                    continue;
                }
                Err(_) => {
                    retries += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(2_u64.pow(retries.min(5))))
                        .await;
                    continue;
                }
            };

            let mut stream = response.bytes_stream();

            while let Some(item) = stream.next().await {
                let chunk = match item {
                    Ok(c) => c,
                    Err(_) => {
                        retries += 1;
                        break; // Retry this range
                    }
                };

                buffer.extend_from_slice(&chunk);

                // Batch write כשהבאפר מלא

                if buffer.len() >= buffer_size {
                    let data = std::mem::take(&mut buffer);
                    let len = data.len() as u64;
                    let offset = current_pos;
                    let file_clone = Arc::clone(&file);

                    tokio::task::spawn_blocking(move || -> Result<()> {
                        #[cfg(unix)]
                        file_clone
                            .write_all_at(&data, offset)
                            .context("Write failed (Unix)")?;

                        #[cfg(windows)]
                        file_clone
                            .seek_write(&data, offset)
                            .context("Write failed (Windows)")?;

                        // הערה: העפנו מפה את ה-sync_data כדי שה-OS ינהל את המטמון!
                        Ok(())
                    })
                    .await
                    .context("Spawn blocking failed")??;

                    current_pos += len;
                    downloaded.fetch_add(len, Ordering::Relaxed);
                    pb.inc(len);
                    retries = 0; // Reset retries on success
                }
            }

            // Flush שאריות
            if !buffer.is_empty() {
                let data = std::mem::take(&mut buffer);
                let len = data.len() as u64;
                let offset = current_pos;
                let file_clone = Arc::clone(&file);

                tokio::task::spawn_blocking(move || -> Result<()> {
                    #[cfg(unix)]
                    file_clone.write_all_at(&data, offset)?;

                    #[cfg(windows)]
                    file_clone.seek_write(&data, offset)?;

                    // גם מפה העפנו את ה-sync_data
                    Ok(())
                })
                .await??;

                current_pos += len;
                downloaded.fetch_add(len, Ordering::Relaxed);
                pb.inc(len);
            }

            // אם הגענו לסוף, יוצאים
            if current_pos > end {
                break;
            }
        }

        Ok(chunk_id)
    }

    // ========== Single stream download (משופר) ==========
    async fn run_single_stream(
        &self,
        url: &str,
        path: &Path,
        total_size: u64,
        start_byte: u64,
        server_etag: Option<String>,
    ) -> Result<()> {
        let mut meta = DownloadMetadata {
            url: url.to_string(),
            etag: server_etag,
            total_size,
            downloaded: start_byte,
            timestamp: chrono::Utc::now().timestamp(),
            chunks_completed: vec![],
        };

        let mut req = self.client.get(url);
        if start_byte > 0 {
            req = req.header(header::RANGE, format!("bytes={}-", start_byte));
        }

        let response = req.send().await.context("Failed to start download")?;

        if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
            bail!("Server returned: {}", response.status());
        }

        let mut stream = response.bytes_stream();

        let file = if start_byte > 0 {
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .context("Failed to open file for append")?;
            f.set_len(start_byte).await?; // חיתוך למיקום הנכון
            f
        } else {
            tokio::fs::File::create(path)
                .await
                .context("Failed to create file")?
        };

        let pb = self.mp.add(ProgressBar::new(total_size));
        pb.set_position(start_byte);
        pb.set_style(
            ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta})")?
            .progress_chars("█▓▒░ ")
        );

        let mut writer = tokio::io::BufWriter::with_capacity(self.buffer_size, file);
        let mut downloaded_this_session = 0u64;

        while let Some(item) = stream.next().await {
            let chunk = item.context("Stream error")?;
            tokio::io::AsyncWriteExt::write_all(&mut writer, &chunk)
                .await
                .context("Failed to write chunk")?;

            let len = chunk.len() as u64;
            downloaded_this_session += len;
            pb.inc(len);

            // עדכון metadata כל 5MB
            if downloaded_this_session % (5 * 1024 * 1024) == 0 {
                meta.downloaded = start_byte + downloaded_this_session;
                meta.save(path)?;
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut writer).await?;
        pb.finish_with_message("✅ Download complete!");

        DownloadMetadata::delete(path)?;
        Ok(())
    }
}

// ========== פונקציית אימות SHA256 ==========
pub fn verify_file(path: &Path, expected_hash: &str) -> Result<bool> {
    println!("{}", "🔍 Verifying file integrity...".yellow());

    let mut file = File::open(path).context("Failed to open file for verification")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024]; // 1MB buffer

    loop {
        let count = file.read(&mut buffer).context("Failed to read file")?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    let result = hasher.finalize();
    let calculated_hash = hex::encode(result);

    if calculated_hash.eq_ignore_ascii_case(expected_hash) {
        println!(
            "{} Hash matches: {}",
            "✅ SECURITY PASS:".green().bold(),
            calculated_hash
        );
        Ok(true)
    } else {
        println!("{} Hash MISMATCH!", "❌ SECURITY ALERT:".red().bold());
        println!("Expected: {}", expected_hash);
        println!("Got:      {}", calculated_hash);
        Ok(false)
    }
}

// ========== Main ==========
#[tokio::main]
async fn main() -> Result<()> {
    let engine = DownloadEngine::new(false, Some(8));

    println!(
        "{}",
        "╔═══════════════════════════════════════════╗"
            .bright_green()
            .bold()
    );
    println!(
        "{}",
        "║   🚀 Nitro Downloader Pro - v2.0 🚀     ║"
            .bright_green()
            .bold()
    );
    println!(
        "{}",
        "║   Resume Support • Multi-Part • Fast     ║".bright_cyan()
    );
    println!(
        "{}",
        "╚═══════════════════════════════════════════╝"
            .bright_green()
            .bold()
    );

    loop {
        print!("\n{} ", "🔗 Enter URL (or 'exit'):".bright_white().bold());
        io::stdout().flush()?;

        let mut url = String::new();
        io::stdin().read_line(&mut url)?;
        let url = url.trim();

        if url.to_lowercase() == "exit" {
            println!("{}", "👋 Goodbye!".bright_yellow());
            break;
        }
        if url.is_empty() {
            continue;
        }

        let filename = url
            .split('/')
            .last()
            .unwrap_or("download.bin")
            .split('?')
            .next()
            .unwrap_or("download.bin");

        let target_path = format!("./downloads/{}", filename);

        match engine.download(url, &target_path).await {
            Ok(_) => {
                println!("\n{}", "═".repeat(50).green());
                println!(
                    "{} {}",
                    "✅ SUCCESS:".green().bold(),
                    "File downloaded successfully!"
                );
                println!("{} {}", "📁 Location:".bright_white(), target_path);
                println!("{}", "═".repeat(50).green());

                // שאל אם רוצים לאמת
                print!(
                    "\n{} ",
                    "🔐 Verify file integrity? Enter SHA256 hash (or press Enter to skip):"
                        .bright_cyan()
                );
                io::stdout().flush()?;

                let mut hash_input = String::new();
                io::stdin().read_line(&mut hash_input)?;
                let hash_input = hash_input.trim();

                if !hash_input.is_empty() {
                    verify_file(Path::new(&target_path), hash_input)?;
                }
            }
            Err(e) => {
                eprintln!("\n{}", "═".repeat(50).red());
                eprintln!("{} {:#}", "❌ ERROR:".red().bold(), e);
                eprintln!("{}", "═".repeat(50).red());
            }
        }
    }

    Ok(())
}
