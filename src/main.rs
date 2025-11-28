use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::io::Write;
use reqwest::blocking::Client;

// 配置文件结构
#[derive(Debug, Deserialize, Serialize)]
struct Config {
    urls: Vec<UrlConfig>,
    concurrency: usize,
    chunk_size: usize,
    report_interval_secs: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct UrlConfig {
    url: String,
    weight: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            urls: vec![
                UrlConfig {
                    url: "https://speed.cloudflare.com/__down?bytes=10000000".to_string(),
                    weight: 50,
                },
                UrlConfig {
                    url: "https://proof.ovh.net/files/10Mb.dat".to_string(),
                    weight: 30,
                },
                UrlConfig {
                    url: "https://ash-speed.hetzner.com/100MB.bin".to_string(),
                    weight: 20,
                },
            ],
            concurrency: 4,
            chunk_size: 65536,
            report_interval_secs: 5,
        }
    }
}

// 统计数据
struct Stats {
    bytes_downloaded: AtomicU64,
    requests_count: AtomicU64,
    errors_count: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            bytes_downloaded: AtomicU64::new(0),
            requests_count: AtomicU64::new(0),
            errors_count: AtomicU64::new(0),
        }
    }

    fn add_bytes(&self, bytes: u64) {
        self.bytes_downloaded.fetch_add(bytes, Ordering::Relaxed);
    }

    fn inc_requests(&self) {
        self.requests_count.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_errors(&self) {
        self.errors_count.fetch_add(1, Ordering::Relaxed);
    }

    fn get_bytes(&self) -> u64 {
        self.bytes_downloaded.load(Ordering::Relaxed)
    }

    fn get_requests(&self) -> u64 {
        self.requests_count.load(Ordering::Relaxed)
    }

    fn get_errors(&self) -> u64 {
        self.errors_count.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        self.bytes_downloaded.store(0, Ordering::Relaxed);
        self.requests_count.store(0, Ordering::Relaxed);
        self.errors_count.store(0, Ordering::Relaxed);
    }
}

// 加载或创建配置文件
fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    if let Ok(content) = fs::read_to_string(path) {
        Ok(toml::from_str(&content)?)
    } else {
        let config = Config::default();
        let toml_string = toml::to_string_pretty(&config)?;
        fs::write(path, toml_string)?;
        println!("✓ 已创建默认配置文件: {}", path);
        Ok(config)
    }
}

// 根据权重选择URL
fn select_url_by_weight(urls: &[UrlConfig]) -> String {
    let total_weight: u32 = urls.iter().map(|u| u.weight).sum();
    let mut random = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() % total_weight as u128) as u32;

    for url_config in urls {
        if random < url_config.weight {
            return url_config.url.clone();
        }
        random -= url_config.weight;
    }

    urls[0].url.clone()
}

// 下载数据块
fn download_chunk(client: &Client, url: &str, chunk_size: usize) -> Result<usize, Box<dyn std::error::Error>> {
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()?;

    let mut buffer = vec![0u8; chunk_size];
    let mut total_read = 0;

    while total_read < chunk_size {
        use std::io::Read;
        match response.read(&mut buffer[total_read..]) {
            Ok(0) => break, // EOF
            Ok(n) => total_read += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(Box::new(e)),
        }
    }

    Ok(total_read)
}

// 工作线程
fn worker_thread(
    thread_id: usize,
    urls: Vec<UrlConfig>,
    chunk_size: usize,
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
) {
    println!("✓ 线程 {} 已启动", thread_id);

    // 为每个线程创建独立的 HTTP 客户端
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(2)
        .build()
        .unwrap();

    while running.load(Ordering::Relaxed) {
        let url = select_url_by_weight(&urls);
        stats.inc_requests();

        match download_chunk(&client, &url, chunk_size) {
            Ok(bytes) => {
                stats.add_bytes(bytes as u64);
            }
            Err(e) => {
                stats.inc_errors();
                eprintln!("线程 {} 错误: {}", thread_id, e);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    println!("✓ 线程 {} 已停止", thread_id);
}

// 报告线程
fn report_thread(
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    interval: Duration,
) {
    let mut last_bytes = 0u64;
    let mut last_time = Instant::now();

    println!("\n按 Ctrl+C 停止测速\n");
    println!("{:-<80}", "");

    while running.load(Ordering::Relaxed) {
        thread::sleep(interval);

        let current_bytes = stats.get_bytes();
        let current_time = Instant::now();
        let elapsed = current_time.duration_since(last_time).as_secs_f64();

        let bytes_diff = current_bytes - last_bytes;
        let speed_mbps = (bytes_diff as f64 * 8.0) / (elapsed * 1_000_000.0);
        let speed_mbytes = bytes_diff as f64 / (elapsed * 1024.0 * 1024.0);

        let total_mb = current_bytes as f64 / (1024.0 * 1024.0);
        let requests = stats.get_requests();
        let errors = stats.get_errors();

        print!("\r");
        print!(
            "📊 速度: {:.2} Mbps ({:.2} MB/s) | 总下载: {:.2} MB | 请求: {} | 错误: {}    ",
            speed_mbps, speed_mbytes, total_mb, requests, errors
        );
        std::io::stdout().flush().unwrap();

        last_bytes = current_bytes;
        last_time = current_time;
    }

    println!("\n{:-<80}", "");
}

// 显示最终统计
fn display_final_stats(stats: &Stats, start_time: Instant) {
    let total_bytes = stats.get_bytes();
    let total_requests = stats.get_requests();
    let total_errors = stats.get_errors();
    let elapsed = start_time.elapsed().as_secs_f64();

    let total_mb = total_bytes as f64 / (1024.0 * 1024.0);
    let avg_speed_mbps = (total_bytes as f64 * 8.0) / (elapsed * 1_000_000.0);
    let avg_speed_mbytes = total_mb / elapsed;

    println!("\n📈 最终统计:");
    println!("{:-<80}", "");
    println!("总运行时间:     {:.2} 秒", elapsed);
    println!("总下载量:       {:.2} MB", total_mb);
    println!("总请求数:       {}", total_requests);
    println!("错误次数:       {}", total_errors);
    println!("成功率:         {:.2}%",
             (total_requests - total_errors) as f64 / total_requests as f64 * 100.0);
    println!("平均速度:       {:.2} Mbps ({:.2} MB/s)", avg_speed_mbps, avg_speed_mbytes);
    println!("{:-<80}", "");
}

// 显示配置信息
fn display_config(config: &Config) {
    println!("⚙️  配置信息:");
    println!("{:-<80}", "");
    println!("并发线程数:     {}", config.concurrency);
    println!("数据块大小:     {} bytes", config.chunk_size);
    println!("报告间隔:       {} 秒", config.report_interval_secs);
    println!("\n📡 测试URL列表:");
    for (i, url_config) in config.urls.iter().enumerate() {
        println!("  {}. {} (权重: {})", i + 1, url_config.url, url_config.weight);
    }
    println!("{:-<80}", "");
}

fn main() {
    println!("🌐 高级多线程网络测速工具");
    println!("=====================================\n");

    // 加载配置
    let config = match load_config("speedtest.toml") {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("❌ 加载配置文件失败: {}", e);
            return;
        }
    };

    display_config(&config);

    // 创建共享状态
    let stats = Arc::new(Stats::new());
    let running = Arc::new(AtomicBool::new(true));
    let start_time = Instant::now();

    // 设置 Ctrl+C 处理
    let running_clone = Arc::clone(&running);
    ctrlc::set_handler(move || {
        println!("\n\n🛑 收到停止信号，正在关闭...");
        running_clone.store(false, Ordering::Relaxed);
    })
        .expect("设置 Ctrl+C 处理器失败");

    // 启动报告线程
    let stats_clone = Arc::clone(&stats);
    let running_clone = Arc::clone(&running);
    let report_handle = thread::spawn(move || {
        report_thread(
            stats_clone,
            running_clone,
            Duration::from_secs(config.report_interval_secs),
        );
    });

    // 启动工作线程
    let mut handles = vec![];
    for i in 0..config.concurrency {
        let urls = config.urls.clone();
        let chunk_size = config.chunk_size;
        let stats_clone = Arc::clone(&stats);
        let running_clone = Arc::clone(&running);

        let handle = thread::spawn(move || {
            worker_thread(i, urls, chunk_size, stats_clone, running_clone);
        });

        handles.push(handle);
    }

    // 等待所有工作线程完成
    for handle in handles {
        handle.join().unwrap();
    }

    // 等待报告线程完成
    report_handle.join().unwrap();

    // 显示最终统计
    display_final_stats(&stats, start_time);

    println!("\n✅ 测速完成!");
}

// Cargo.toml 依赖:
// [dependencies]
// reqwest = { version = "0.11", features = ["blocking"] }
// serde = { version = "1.0", features = ["derive"] }
// toml = "0.8"
// ctrlc = "3.4"
