pub mod thread_pool;

use std::{fs::{self, File}, io::{BufWriter, Write}, path::Path, sync::{Arc, atomic::{AtomicBool, AtomicU32, Ordering}, mpsc::{self, TryRecvError}}, thread, time::Duration};
use slint::SharedString;
use url::Url;
use reqwest::blocking::Client;
use crate::thread_pool::ThreadPool;

slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;

    // 下载管理器
    let download_management = DownloadManagement::new();

    window.on_select_dir({
        let ui_weak = window.as_weak();
        move || {
            ui_weak.upgrade().unwrap().set_work_dir(show_open_dialog());
        }
    });

    // 暂停下载
    window.on_pause_download({
        let pause = Arc::clone(&download_management.pause);
        move || {
            println!("pause");
            pause.store(true, Ordering::Relaxed);
        }
    });

    // 取消下载
    window.on_cancel_download({
        let ui_weak = window.as_weak();
        let cancel = Arc::clone(&download_management.cancel);
        let downloaded_files = Arc::clone(&download_management.downloaded_files);
        let pause = Arc::clone(&download_management.pause);
        let running = Arc::clone(&download_management.running);
        move || {
            println!("cancel");

            // 暂停时取消下载
            if running.load(Ordering::Relaxed) && pause.load(Ordering::Relaxed) {
                downloaded_files.store(0, Ordering::Relaxed);

                let ui = ui_weak.unwrap();
                ui.set_message("canceled.".into());
                ui.set_progress(0);
                ui.set_percent(0.0);
                ui.set_total(SharedString::new());
                ui.set_is_pause(false);
                ui.set_enable_start_btn(true);
                ui.set_enable_pause_btn(false);
                ui.set_enable_cancel_btn(false);
            } else {
                // 下载时取消下载
                cancel.store(true, Ordering::Relaxed);
            }
        }
    });

    // 启动下载
    window.on_start_download({
        let ui_weak = window.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            
            let threads = ui.get_threads() as usize;
            let retry = ui.get_retry();
            let timeout = ui.get_timeout() as u64;
            let m3u8_url = ui.get_m3u8_url();

            let save_dir = format!("{}\\{}", ui.get_work_dir(), ui.get_video_dir());
            if !Path::new(&save_dir).is_dir() && fs::create_dir_all(&save_dir).is_err() {
                ui.set_message("Failed to create directory.".into());
                return;
            }

            let ui_weak = ui.as_weak();
            let downloaded_files = Arc::clone(&download_management.downloaded_files);

            let running = Arc::clone(&download_management.running);
            let pause = Arc::clone(&download_management.pause);
            let cancel = Arc::clone(&download_management.cancel);

            if !running.load(Ordering::Relaxed) {
                running.store(true, Ordering::Relaxed);
                ui.set_progress(0);
                ui.set_percent(0.0);
                ui.set_total(SharedString::new());
            }
            pause.store(false, Ordering::Relaxed);
            cancel.store(false, Ordering::Relaxed);

            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                match resolve_m3u8(&m3u8_url, &save_dir, retry, timeout) {
                    Ok(wait_download_files) => {
                        let file_nums = wait_download_files.len() as u32;
                        let start_index = downloaded_files.load(Ordering::Relaxed);
                        ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_progress(start_index as i32);
                            ui.set_total(format!(" / {}", file_nums).into());
                        }).unwrap();

                        // 处理暂停再继续时，文件已下载完毕
                        if start_index >= file_nums {
                            println!("start_index=file_nums");
                            ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.set_message("The download has been completed.".into());
                                ui.set_progress(0);
                                ui.set_total(SharedString::new());
                                ui.set_enable_start_btn(true);
                                ui.set_enable_pause_btn(false);
                                ui.set_enable_cancel_btn(false);
                            }).unwrap();
                        } else {
                            let pool = ThreadPool::new(threads);

                            for index in start_index..file_nums {
                                let downloaded_files_clone = Arc::clone(&downloaded_files);
                                let pause_clone = Arc::clone(&pause);
                                let cance_clonel = Arc::clone(&cancel);
                                let ui_weak_clone = ui_weak.clone();

                                let client = Client::builder().connect_timeout(Duration::from_secs(timeout)).build().unwrap();
                                let filename = wait_download_files.get(index as usize).unwrap().filename.to_string();
                                let url = wait_download_files.get(index as usize).unwrap().url.to_string();

                                let tx_clone = tx.clone();
                                
                                pool.execute(move || {
                                    // 暂停下载
                                    if pause_clone.load(Ordering::Relaxed) {
                                        ui_weak_clone.upgrade_in_event_loop(move |ui| {
                                            ui.set_message("Paused.".into());
                                            ui.set_is_pause(true);
                                            ui.set_enable_start_btn(true);
                                            ui.set_enable_pause_btn(false);
                                            ui.set_enable_cancel_btn(true);
                                        }).unwrap();
                                        return;
                                    }
                                    // 取消下载
                                    if cance_clonel.load(Ordering::Relaxed) {
                                        ui_weak_clone.upgrade_in_event_loop(move |ui| {
                                            ui.set_message("Canceled.".into());
                                            ui.set_progress(0);
                                            ui.set_percent(0.0);
                                            ui.set_total(SharedString::new());
                                            ui.set_enable_start_btn(true);
                                            ui.set_enable_pause_btn(false);
                                            ui.set_enable_cancel_btn(false);
                                        }).unwrap();
                                        return;
                                    }

                                    for attempt in 0..retry {
                                        if let Ok(resp) = client.get(url.as_str()).send() && resp.status().is_success() {
                                            let content = resp.bytes().unwrap();
                                            let mut file = File::create(filename.as_str()).unwrap();
                                            file.write_all(&content).unwrap();
                                            downloaded_files_clone.fetch_add(1, Ordering::Relaxed);
                                            let percent = ((downloaded_files_clone.load(Ordering::Relaxed) as f32 / file_nums as f32) * 100.0).floor();
                                            tx_clone.send((percent, downloaded_files_clone.load(Ordering::Acquire))).unwrap();
                                            break;
                                        }

                                        if attempt < retry - 1 {
                                            thread::sleep(Duration::from_millis(100));
                                        }
                                    }
                                });
                            }
                        }
                    },
                    Err(e) => {
                        let err = e.to_string();
                        ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_message(format!("{err}").into());
                            ui.set_enable_start_btn(true);
                            ui.set_enable_pause_btn(false);
                            ui.set_enable_cancel_btn(false);
                        }).unwrap();
                    }
                }
            });

            // 监控下载进度
            let ui_weak_rx = ui.as_weak();
            let downloaded_files_rx = Arc::clone(&download_management.downloaded_files);
            let downloaded_running_rx = Arc::clone(&download_management.running);
            thread::spawn(move || {
                loop {
                    match rx.try_recv() {
                        Ok((percent, download_file_nums)) => {
                            let downloaded_files_rx = Arc::clone(&downloaded_files_rx);
                            let downloaded_running_rx = Arc::clone(&downloaded_running_rx);
                            ui_weak_rx.upgrade_in_event_loop(move |ui| {
                                ui.set_progress(download_file_nums as i32);
                                // todo: 在下载中取消下载时进度应为0
                                ui.set_percent(percent / 100.0);

                                if percent == 100.0 {
                                    downloaded_files_rx.store(0, Ordering::Relaxed);
                                    downloaded_running_rx.store(false, Ordering::Relaxed);
                                    ui.set_message("Download Finished.".into());
                                    ui.set_enable_start_btn(true);
                                    ui.set_enable_pause_btn(false);
                                    ui.set_enable_cancel_btn(false);
                                }
                            }).unwrap();
                        },
                        Err(TryRecvError::Empty) => {
                            println!("TryRecvError::Empty");
                            thread::sleep(Duration::from_millis(250));
                        },
                        Err(TryRecvError::Disconnected) => {
                            println!("TryRecvError::Disconnected");
                            break;
                        }
                    }
                }
            });
        }
    });

    window.run()
}

struct DownloadManagement {
    downloaded_files: Arc<AtomicU32>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl DownloadManagement {
    fn new() -> Self {
        Self {
            downloaded_files: Arc::new(AtomicU32::new(0)),
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct WaitDownloadFile {
    filename: String,
    url: String,
}

impl WaitDownloadFile {
    fn new(filename: String, url: String) -> Self {
        Self { filename, url }
    }
}

fn show_open_dialog() -> SharedString {
    rfd::FileDialog::new().pick_folder().map(|path| path.to_string_lossy().to_string().into()).unwrap_or_default()
}

fn resolve_m3u8(
    m3u8_url: &SharedString,
    save_dir: &str,
    retry: i32,
    timeout: u64
) -> Result<Vec<WaitDownloadFile>, Box<dyn std::error::Error>> {
    let parsed_url = Url::parse(m3u8_url.as_str())?;
    let base_url = parsed_url.join(".")?;
    let mut contents = String::new();
    let mut is_timeout = false;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(timeout))
        .build()
        .unwrap();

    for _ in 0..retry {
        if let Ok(res) = client.get(m3u8_url.as_str()).send() && res.status().is_success() {
            contents = res.text()?;
            is_timeout = false;
            break;
        } else {
            is_timeout = true;
        }
    }

    if is_timeout {
        return Err("Connection timeout".into());
    }

    if contents.is_empty() {
        return Err("Not valid M3U8 URL".into());
    }

    let m3u8_file = File::create(Path::new(save_dir).join("index.m3u8"))?;
    let mut writer = BufWriter::new(m3u8_file);
    let mut wait_download_files: Vec<WaitDownloadFile> = Vec::new();

    for line in contents.lines() {
        if line.starts_with("#EXT-X-KEY") {
            let key = line.split("URI=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
            let replace_key = line.replace(key, "key.key");
            let key_file = Path::new(save_dir).join(&replace_key).to_string_lossy().to_string();

            let key_url = if key.starts_with("http://") || key.starts_with("https://") {
                key.to_string()
            } else {
                base_url.join(line)?.to_string()
            };

            wait_download_files.push(WaitDownloadFile::new(key_file, key_url));
            writeln!(writer, "{}", replace_key)?;
            continue;
        }
        
        if !line.starts_with("#") {
            let (filename, ts_name, url) = if line.starts_with("http://") || line.starts_with("https://") {
                let ts_name = line.split("/").last().unwrap();
                let filename = Path::new(save_dir).join(&ts_name).to_string_lossy().to_string();
                (filename, ts_name, line.to_string())
            } else {
                let url = base_url.join(line)?.to_string();
                let filename = Path::new(save_dir).join(line).to_string_lossy().to_string();
                (filename, line, url)
            };

            wait_download_files.push(WaitDownloadFile::new(filename, url));
            writeln!(writer, "{}", ts_name)?;
            continue;
        }

        writeln!(writer, "{}", line)?;
    }

    Ok(wait_download_files)
}