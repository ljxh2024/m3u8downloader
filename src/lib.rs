pub mod thread_pool;

use std::{fs::{self, File}, io::{BufWriter, Write}, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, Ordering}, mpsc::{self, TryRecvError}}, thread, time::Duration};
use slint::SharedString;
use url::Url;
use reqwest::blocking::Client;
use crate::thread_pool::ThreadPool;

slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;

    let downloading = Arc::new(AtomicBool::new(false));
    let is_pause = Arc::new(AtomicBool::new(false));
    let is_new_task = Arc::new(AtomicBool::new(true));
    let is_cancel = Arc::new(AtomicBool::new(false));
    let total_nums = Arc::new(AtomicU32::new(0)); // 总文件数
    // 已下载的文件列表，存储download_url
    let downloaded_files: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    // 线程管理
    let thread_handles: Arc<Mutex<Option<Vec<thread::JoinHandle<()>>>>> = Arc::new(Mutex::new(Some(vec![])));

    // 选择目录
    window.on_select_dir({
        let ui_weak = window.as_weak();
        move || {
            ui_weak.upgrade().unwrap().set_work_dir(show_open_dialog());
        }
    });

    // 暂停下载
    window.on_pause_download({
        let ui_weak = window.as_weak();
        let is_new_task = Arc::clone(&is_new_task);
        let is_pause = Arc::clone(&is_pause);
        let thread_handles_clone_pause = Arc::clone(&thread_handles);
        let downloading_clone = Arc::clone(&downloading);

        move || {
            downloading_clone.store(false, Ordering::Relaxed);
            is_new_task.store(false, Ordering::Relaxed); // 表示不是一个新下载任务
            is_pause.store(true, Ordering::Relaxed);

            let ui = ui_weak.unwrap();
            set_ui_with_pause(&ui, SharedString::from("You paused the download."));
            release_threads(&thread_handles_clone_pause);
        }
    });

    // 取消下载
    window.on_cancel_download({
        let ui_weak = window.as_weak();
        let is_new_task = Arc::clone(&is_new_task);
        let is_pause = Arc::clone(&is_pause);
        let is_cancel = Arc::clone(&is_cancel);
        let downloaded_files_clone = Arc::clone(&downloaded_files);
        let thread_handles_clone_cancel = Arc::clone(&thread_handles);
        let downloading_clone = Arc::clone(&downloading);

        move || {
            // 清空并立即释放锁
            downloaded_files_clone.lock().unwrap().clear();

            downloading_clone.store(false, Ordering::Relaxed);
            is_new_task.store(true, Ordering::Relaxed);
            is_pause.store(false, Ordering::Relaxed);
            is_cancel.store(true, Ordering::Relaxed);

            let ui = ui_weak.unwrap();
            reset_ui_with_default(&ui, SharedString::from("You canceled the download."), true);

            release_threads(&thread_handles_clone_cancel);
        }
    });

    // 启动新的（或继续）下载任务
    window.on_start_download({
        let ui_weak = window.as_weak();
        let is_new_task = Arc::clone(&is_new_task);
        let is_pause = Arc::clone(&is_pause);
        let is_cancel = Arc::clone(&is_cancel);
        let downloaded_files_clone = Arc::clone(&downloaded_files);
        let thread_handles_download = Arc::clone(&thread_handles);
        let total_nums_clone = Arc::clone(&total_nums);
        let downloading_clone = Arc::clone(&downloading);

        move || {
            is_pause.store(false, Ordering::Relaxed);
            is_cancel.store(false, Ordering::Relaxed);

            let ui = ui_weak.unwrap();

            ui.set_message(SharedString::from("Parsing M3U8..."));

            let threads = ui.get_threads().parse::<usize>().unwrap_or(4);
            let retry = ui.get_retry().parse::<u32>().unwrap_or(3);
            let timeout = ui.get_retry().parse::<u64>().unwrap_or(5);
            let m3u8_url = ui.get_m3u8_url();

            // 保存目录
            let save_dir = format!("{}\\{}", ui.get_work_dir(), ui.get_video_dir());
            if !Path::new(&save_dir).is_dir() && fs::create_dir_all(&save_dir).is_err() {
                ui.set_message("Failed to create directory.".into());
                return;
            }

            let (tx, rx) = mpsc::channel(); // 使用通道传递进度信息
            let ui_weak_1 = ui.as_weak();
            let is_new_task_clone = Arc::clone(&is_new_task);
            let is_pause_clone = Arc::clone(&is_pause);
            let is_cancel_clone = Arc::clone(&is_cancel);
            let downloaded_files_clone = Arc::clone(&downloaded_files_clone);
            let total_nums_clone = Arc::clone(&total_nums_clone);
            let downloading_clone = Arc::clone(&downloading_clone);

            // 使用新线程下载文件
            let handle_download = thread::spawn(move || {
                match resolve_m3u8(&m3u8_url, &save_dir, retry, timeout) {
                    Ok(wait_download_files) => {
                        // 需要下载的文件列表
                        let new_files: Vec<WaitDownloadFile> = if is_new_task_clone.load(Ordering::Relaxed) {
                            wait_download_files
                        } else {
                            let finished_files = downloaded_files_clone.lock().unwrap();
                            // 过滤已下载的文件
                            wait_download_files.into_iter().filter(|item| !finished_files.contains(&item.download_url)).collect()
                        };
                        // 总文件数
                        let total_file_nums = if is_new_task_clone.load(Ordering::Relaxed) {
                            let len = new_files.len() as u32;
                            total_nums_clone.store(len, Ordering::Relaxed);
                            len
                        } else {
                            total_nums_clone.load(Ordering::Relaxed)
                        };
                        
                        // 下载前重置部分UI
                        ui_weak_1.upgrade_in_event_loop(move |ui| {
                            ui.set_message(SharedString::from("Downloading..."));
                            ui.set_is_pause(false);
                            ui.set_enable_pause_btn(true); // 允许暂停
                            ui.set_enable_cancel_btn(true); // 允许取消
                            ui.set_total(SharedString::from(format!(" / {}",total_file_nums)));
                        }).unwrap();
                        
                        downloading_clone.store(true, Ordering::Relaxed);

                        let pool = ThreadPool::new(threads);
                        for item in new_files {
                            let tx_clone = tx.clone();
                            let is_pause_clone = Arc::clone(&is_pause_clone);
                            let is_cancel_clone = Arc::clone(&is_cancel_clone);
                            let client = Client::builder().connect_timeout(Duration::from_secs(timeout)).build().unwrap();
                            let downloaded_files_clone = Arc::clone(&downloaded_files_clone);
                            pool.execute(move || {
                                if !is_pause_clone.load(Ordering::Relaxed) && !is_cancel_clone.load(Ordering::Relaxed) {
                                    let mut is_finish = false;
                                    // 带重试的下载
                                    for attempt in 0..retry {
                                        if let Ok(resp) = client.get(&item.download_url).send() && resp.status().is_success() {
                                            let content = resp.bytes().unwrap();
                                            let mut file = File::create(&item.save_path).unwrap();
                                            file.write_all(&content).unwrap();

                                            // 记录已下载的文件
                                            let n = {
                                                let mut downloaded = downloaded_files_clone.lock().unwrap();
                                                downloaded.push(item.download_url.to_owned());
                                                downloaded.len()
                                            };
                                            // 用以进度条展示
                                            let percent = ((n as f32 / total_file_nums as f32) * 100.0).floor();
                                            tx_clone.send(ChannelMessage::Schedule(n as i32, percent as u32)).unwrap();

                                            is_finish = true;
                                            break;
                                        }
                                        if attempt < retry - 1 {
                                            tx_clone.send(ChannelMessage::Retry(SharedString::from(format!("Retrying to download: {}", &item.download_url)))).unwrap();
                                            thread::sleep(Duration::from_millis(200));
                                        }
                                    }
                                    if !is_finish {
                                        is_pause_clone.store(true, Ordering::Relaxed);
                                        tx_clone.send(ChannelMessage::Pause(format!("Download failed: {}", &item.download_url))).unwrap();
                                    }
                                }
                            });
                        }
                    },
                    Err(e) => {
                        let err_msg = e.to_string();
                        ui_weak_1.upgrade_in_event_loop(move |ui| {
                            reset_ui_with_default(&ui, SharedString::from(err_msg), false);
                        }).unwrap();
                    }
                }
            });

            // 监控下载进度
            let ui_weak_monitor = ui.as_weak();
            let is_new_task = Arc::clone(&is_new_task);
            let downloaded_files_clone = Arc::clone(&downloaded_files);
            let downloading_clone = Arc::clone(&downloading);
            let handle_monitor = thread::spawn(move || {
                download_monitor(is_new_task, downloaded_files_clone, downloading_clone, ui_weak_monitor, rx);
            });

            // 收集线程
            let mut thread_handles = thread_handles_download.lock().unwrap();
            if let Some(handles) = thread_handles.as_mut() {
                handles.push(handle_download);
                handles.push(handle_monitor);
            }
        }
    });

    window.run()
}

// 待下载文件信息
#[derive(Debug)]
struct WaitDownloadFile {
    save_path: String, // 保存路径
    download_url: String, // 下载链接
}

impl WaitDownloadFile {
    fn new(save_path: String, download_url: String) -> Self {
        Self { save_path, download_url }
    }
}

fn show_open_dialog() -> SharedString {
    rfd::FileDialog::new().pick_folder().map(|path| path.to_string_lossy().to_string().into()).unwrap_or_default()
}

// 解析M3U8，并把待下载文件放入队列中
fn resolve_m3u8(
    m3u8_url: &SharedString,
    save_dir: &str,
    retry: u32,
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

    // 超时
    if is_timeout {
        return Err("Connection timeout.".into());
    }

    // 内容为空
    if contents.is_empty() {
        return Err("Not valid M3U8 URL.".into());
    }

    let m3u8_file = File::create(Path::new(save_dir).join("index.m3u8"))?;
    let mut writer = BufWriter::new(m3u8_file);
    let mut wait_download_files: Vec<WaitDownloadFile> = Vec::new(); // 等待下载的文件列表

    for line in contents.lines() {
        if line.starts_with("#EXT-X-KEY") {
            let key = line.split("URI=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
            let (filename, url) = parse_m3u8_key_segment(&base_url, key)?;
            wait_download_files.push(WaitDownloadFile::new(format!("{}\\{}", save_dir, filename), url));
            writeln!(writer, "{}", line.replace(key, &filename))?;
        } else if !line.starts_with("#") && !line.trim().is_empty() {
            let (filename, url) = parse_m3u8_key_segment(&base_url, line)?;
            if !filename.trim().is_empty() {
                wait_download_files.push(WaitDownloadFile::new(format!("{}\\{}", save_dir, filename), url));
                writeln!(writer, "{}", filename)?;
            }
        } else {
            if !line.trim().is_empty() {
                writeln!(writer, "{}", line)?;
            }
        }
    }

    // 内容未提取到可下载的文件
    if wait_download_files.is_empty() {
        return Err("No downloadable files found.".into());
    }

    Ok(wait_download_files)
}

// 解析m3u8里面的key与切片
fn parse_m3u8_key_segment(base_url: &Url, s: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let url = if s.starts_with("http://") || s.starts_with("https://") {
        Url::parse(s)?
    } else {
        base_url.join(s)?
    };

    let filename = url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .ok_or_else(|| format!("URL path has no filename: {}", url))? 
        .to_string();

    Ok((filename, url.to_string()))
}

// 枚举通道消息
#[derive(Debug)]
enum ChannelMessage {
    Pause(String),
    Retry(SharedString),
    Schedule(i32, u32), // 进度，百分比和已下载文件数
}

// 下载监控
fn download_monitor(
    is_new_task: Arc<AtomicBool>,
    downloaded_files: Arc<Mutex<Vec<String>>>,
    downloading: Arc<AtomicBool>,
    ui_weak: slint::Weak<AppWindow>,
    receiver: mpsc::Receiver<ChannelMessage>
) {
    loop {
        match receiver.try_recv() {
            Ok(channel_message) => {
                // 通道消息类型
                match channel_message {
                    // 下载进度
                    ChannelMessage::Schedule(progress, percent) => {
                        // println!("progress:{}, percent:{}", progress, percent);
                        if downloading.load(Ordering::Relaxed) {
                            ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.set_progress(progress);
                                ui.set_percent(percent as f32 / 100.0);
                            }).unwrap();
                        }
                        
                        // 下载完成
                        if percent >= 100 {
                            is_new_task.store(true, Ordering::Relaxed);
                            downloading.store(false, Ordering::Relaxed);
                            downloaded_files.lock().unwrap().clear();
                            ui_weak.upgrade_in_event_loop(move |ui| {
                                reset_ui_with_default(&ui, SharedString::from("Download finished."), false);
                            }).unwrap();
                        }
                    },
                    // 暂停
                    ChannelMessage::Pause(msg) => {
                        // 同暂停按钮事件回调一致
                        is_new_task.store(false, Ordering::Relaxed);
                        downloading.store(false, Ordering::Relaxed);
                        ui_weak.upgrade_in_event_loop(move |ui| {
                            set_ui_with_pause(&ui, SharedString::from(msg));
                        }).unwrap();
                        // break;
                    },
                    // 重试
                    ChannelMessage::Retry(msg) => {
                        ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_message(msg);
                        }).unwrap();
                    }
                }
            },
            Err(TryRecvError::Empty) => {
                // println!("TryRecvError::Empty");
                thread::sleep(Duration::from_millis(250));
            },
            Err(TryRecvError::Disconnected) => {
                println!("TryRecvError::Disconnected");
                break;
            }
        }
    }
}

// 释放线程
fn release_threads(thread_handles: &Arc<Mutex<Option<Vec<thread::JoinHandle<()>>>>>) {
    let thread_handles = thread_handles.lock().unwrap().take();
    if let Some(handles) = thread_handles {
        for handle in handles {
            handle.join().unwrap();
        }
    }
}

// 重置UI变量
fn reset_ui_with_default(
    ui: &AppWindow,
    message: SharedString,
    is_reset_progress: bool
) {
    ui.set_enable_start_btn(true);
    ui.set_enable_pause_btn(false);
    ui.set_enable_cancel_btn(false);
    ui.set_is_pause(false);
    ui.set_message(message);

    // 清空下载进度
    if is_reset_progress {
        ui.set_total(SharedString::new());
        ui.set_progress(0);
        ui.set_percent(0.0);
    }
}

// 暂停时UI变量状态
fn set_ui_with_pause(ui: &AppWindow, message: SharedString) {
    ui.set_message(message);
    ui.set_is_pause(true);
    ui.set_enable_start_btn(true);
    ui.set_enable_pause_btn(false);
    ui.set_enable_cancel_btn(true);
}