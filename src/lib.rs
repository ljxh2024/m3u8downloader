pub mod thread_pool;

use std::{fs::{self, File, OpenOptions}, io::{BufWriter, Write}, os::windows::process::CommandExt, path::Path, process::Command, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc::{self, TryRecvError}}, thread, time::{Duration, Instant}};
use slint::SharedString;
use url::Url;
use reqwest::blocking::Client;
use crate::thread_pool::ThreadPool;

slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;

    let (tx, rx) = mpsc::channel();

    // 启动新的下载任务
    window.on_start_download({
        let ui_weak = window.as_weak();
        let tx_start = tx.clone();
        move || {
            let ui = ui_weak.unwrap();

            ui.set_enable_download_btn(false);
            ui.set_is_downloading(true);
            ui.set_has_failed_file(false);
            ui.set_message(SharedString::from("Parsing..."));

            tx_start.send(ChannelMessage::Start(DownloadConfig {
                work_dir: ui.get_work_dir(),
                video_dir: ui.get_video_dir(),
                threads: ui.get_threads().parse::<usize>().unwrap_or(4),
                retry: ui.get_retry().parse::<u32>().unwrap_or(3),
                timeout: ui.get_retry().parse::<u64>().unwrap_or(5),
                m3u8_url: ui.get_m3u8_url(),
                is_convert: ui.get_is_convert(),
                is_delete_slice: ui.get_is_delete_slice(),
            })).unwrap();
        }
    });

    // 暂停下载
    window.on_pause_download({
        let ui_weak = window.as_weak();
        let tx_pause = tx.clone();
        move || {
            let ui = ui_weak.unwrap();
            ui.set_enable_pause_btn(false);
            ui.set_message(SharedString::from("Pausing..."));

            tx_pause.send(ChannelMessage::Pause).unwrap();
        }
    });

    // 取消下载
    window.on_cancel_download({
        let ui_weak = window.as_weak();
        let tx_cancel = tx.clone();
        move || {
            let ui = ui_weak.unwrap();
            ui.set_enable_cancel_btn(false);
            ui.set_message(SharedString::from("Canceling..."));
            
            tx_cancel.send(ChannelMessage::Cancel).unwrap();
        }
    });

    // 选择工作目录
    window.on_select_dir({
        let ui_weak = window.as_weak();
        move || {
            ui_weak
                .upgrade()
                .unwrap()
                .set_work_dir(rfd::FileDialog::new()
                    .pick_folder()
                    .map(|path| path.to_string_lossy().to_string().into())
                    .unwrap_or_default()
                );
        }
    });

    // 打开目录
    window.on_open_failed_dir({
        let ui_weak = window.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            Command::new("explorer").arg(Path::new(&format!("/select,{}\\{}\\failed_link.txt", ui.get_work_dir(), ui.get_video_dir()))).spawn().unwrap();
        }
    });

    // 任务调度/UI非阻塞更新线程
    let ui_weak = window.as_weak();
    thread::spawn(move || {
        let downloader = Arc::new(Downloader::new());
        loop {
            match rx.try_recv() {
                Ok(channel_message) => {
                    // 通道消息类型
                    match channel_message {
                        // 已下载文件数
                        ChannelMessage::Downloaded(nums) => {
                            ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.set_downloaded_nums(nums);
                            }).unwrap();
                        },
                        // 处理下载
                        ChannelMessage::Start(download_config) => {
                            let tx_monitor = tx.clone();
                            let downloader_monitor = Arc::clone(&downloader);
                            let ui_weak_clone = ui_weak.clone();
                            // 启动新线程下载,避免阻塞UI更新
                            thread::spawn(move || {
                                let start = Instant::now();
                                let downloader_task = Arc::clone(&downloader_monitor);
                                let ui_weak_task = ui_weak_clone.clone();

                                // 下载成功转MP4
                                let args = if download_config.is_convert {
                                    let save_dir = format!("{}\\{}", download_config.work_dir, download_config.video_dir);
                                    vec!["-i".to_string(), format!("{}\\index.m3u8", save_dir), "-c".to_string(), "copy".to_string(), format!("{}\\output.mp4", save_dir)]
                                } else {
                                    vec![]
                                };

                                let handle = thread::spawn(move || {
                                    if let Err(e) = start_download(tx_monitor, downloader_task, download_config) {
                                        let err_msg = e.to_string();
                                        println!("{err_msg}");
                                        ui_weak_task.upgrade_in_event_loop(move |ui| {
                                            ui.set_message(SharedString::from(err_msg));
                                        }).unwrap();
                                    };
                                });
                                handle.join().unwrap();
                                let duration = start.elapsed();
                                println!("duration:{:.2}", duration.as_secs_f64());

                                // println!("is_pause:{}", downloader_monitor.is_pause.load(Ordering::Relaxed));
                                // 暂停成功后清理
                                if downloader_monitor.is_pause.load(Ordering::Relaxed) {
                                    downloader_monitor.is_new_download.store(false, Ordering::Relaxed);
                                    ui_weak_clone.upgrade_in_event_loop(move |ui| {
                                        ui.set_message(SharedString::from("Paused."));
                                        ui.set_enable_download_btn(true);
                                        ui.set_is_pause(true);
                                    }).unwrap();
                                } else if downloader_monitor.is_cancel.load(Ordering::Relaxed) {
                                    // 下载中的取消处理
                                    reset_download_status(&ui_weak_clone, &downloader_monitor, SharedString::from("Canceled."), true);
                                } else {
                                    // if is_success {
                                    //     let mut msg: &str = "Download Finished";
                                    //     if !args.is_empty() {
                                    //         ui_weak_clone.upgrade_in_event_loop(move |ui| {
                                    //             ui.set_message(SharedString::from("Converting to mp4..."));
                                    //         }).unwrap();
                                    //         if Command::new("ffmpeg").args(&args).creation_flags(0x08000000).output().unwrap().status.success() {
                                    //             msg = "Successfully converted to mp4.";
                                    //             // :todo
                                    //             // let f = downloader.finished_files.lock().unwrap(); todo
                                    //         } else {
                                    //             msg = "Unable to convert to mp4.";
                                    //         }
                                    //     }
                                    //     reset_download_status(&ui_weak_clone, &downloader_monitor, SharedString::from(msg), false);
                                    // }
                                    // reset_download_status(&ui_weak_clone, &downloader_monitor, SharedString::from("Download Finished."), false);
                                    let mut msg: &str = "Download Finished";
                                    if !args.is_empty() {
                                        ui_weak_clone.upgrade_in_event_loop(move |ui| {
                                            ui.set_message(SharedString::from("Converting to mp4..."));
                                        }).unwrap();
                                        if Command::new("ffmpeg").args(&args).creation_flags(0x08000000).output().unwrap().status.success() {
                                            msg = "Successfully converted to mp4.";
                                            // :todo
                                            // let f = downloader.finished_files.lock().unwrap(); todo
                                        } else {
                                            msg = "Unable to convert to mp4.";
                                        }
                                    }
                                    reset_download_status(&ui_weak_clone, &downloader_monitor, SharedString::from(msg), false);
                                }
                            });
                        },
                        // 解析完M3U8,开始下载key及切片
                        ChannelMessage::Downloading(total_nums) => {
                            downloader.is_pause.store(false, Ordering::Relaxed);
                            downloader.is_cancel.store(false, Ordering::Relaxed);

                            ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.set_is_pause(false);
                                ui.set_enable_download_btn(false);
                                ui.set_enable_pause_btn(true);
                                ui.set_enable_cancel_btn(true);
                                ui.set_total_nums(total_nums);
                                ui.set_message(SharedString::from("Downloading..."));
                            }).unwrap();
                        },
                        // 暂停
                        ChannelMessage::Pause => {
                            println!("ChannelMessage::Pause");
                            downloader.is_pause.store(true, Ordering::Relaxed);
                        },
                        // 取消(分下载中的取消和暂停中的取消)
                        ChannelMessage::Cancel => {
                            println!("ChannelMessage::Cancel");
                            downloader.is_cancel.store(true, Ordering::Relaxed);
                            if downloader.is_pause.load(Ordering::Relaxed) {
                                reset_download_status(&ui_weak, &downloader, SharedString::from("Canceled."), true);
                            }
                        }
                    }
                },
                Err(TryRecvError::Empty) => {
                    // println!("TryRecvError::Empty");
                    // thread::sleep(Duration::from_millis(200));
                },
                Err(TryRecvError::Disconnected) => {
                    // println!("TryRecvError::Disconnected");
                    break;
                }
            }
        }
    });

    window.run()
}

// 枚举通道消息
#[derive(Debug)]
enum ChannelMessage {
    Start(DownloadConfig),
    Downloading(i32), // 参数为总文件数
    Downloaded(i32), // 参数为已下载文件数
    Pause,
    Cancel,
    // Downloaded(u32), // 已下载文件数
}

// 待下载文件信息
#[derive(Debug)]
struct WaitDownloadFile {
    filename: String, // 文件名
    save_path: String, // 保存目录
    download_url: String, // 下载链接
}

impl WaitDownloadFile {
    fn new(filename:String, save_path: String, download_url: String) -> Self {
        Self {
            filename,
            save_path,
            download_url,
        }
    }
}

// 下载队列信息
struct Downloader {
    is_new_download: AtomicBool,
    is_pause: AtomicBool,
    is_cancel: AtomicBool,
    file_total_nums: AtomicUsize,
    finished_files: Mutex<Vec<String>>, // 已下载完成的文件列表，存储文件名
    failed_files: Mutex<Vec<String>>, // 下载失败的文件列表，存储url
}

impl Downloader {
    fn new() -> Self {
        Self {
            is_new_download: AtomicBool::new(true),
            is_pause: AtomicBool::new(false),
            is_cancel: AtomicBool::new(false),
            file_total_nums: AtomicUsize::new(0),
            finished_files: Mutex::new(vec![]),
            failed_files: Mutex::new(vec![]),
        }
    }
}

#[derive(Debug)]
struct DownloadConfig {
    work_dir: SharedString,
    video_dir: SharedString,
    threads: usize,
    retry: u32,
    timeout: u64,
    m3u8_url: SharedString,
    is_convert: bool,
    is_delete_slice: bool,
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
        // .danger_accept_invalid_certs(true)
        .user_agent("Chrome/147)")
        .build()
        .unwrap();
    // println!("m3u8_url{}", m3u8_url);

    for _ in 0..retry {
        if let Ok(res) = client.get(m3u8_url.as_str()).send() && res.status().is_success() {
            // println!("66{:?}", res.bytes()?);
            contents = res.text()?;
            is_timeout = false;
            break;
        }
    }
    // println!("contents{}", contents);

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
            wait_download_files.push(WaitDownloadFile::new(filename.to_owned(), save_dir.to_owned(), url));
            writeln!(writer, "{}", line.replace(key, &filename))?;
        } else if !line.starts_with("#") && !line.trim().is_empty() {
            let (filename, url) = parse_m3u8_key_segment(&base_url, line)?;
            if !filename.trim().is_empty() {
                wait_download_files.push(WaitDownloadFile::new(filename.to_owned(), save_dir.to_string(), url));
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

// 启动下载
fn start_download(
    tx: mpsc::Sender<ChannelMessage>,
    downloader: Arc<Downloader>,
    download_config: DownloadConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    // 保存目录
    let save_dir = format!("{}\\{}", download_config.work_dir, download_config.video_dir);
    if !Path::new(&save_dir).is_dir() && fs::create_dir_all(&save_dir).is_err() {
        return Err("Failed to create directory.".into());
    }

    // println!("{:?}",save_dir);

    // 解析M3U8
    let resolved =  resolve_m3u8(&download_config.m3u8_url, &save_dir, download_config.retry, download_config.timeout)?;
    // 待下载文件和总文件数
    let (files, file_total_nums) = if downloader.is_new_download.load(Ordering::Relaxed) {
        let len = resolved.len();
        // 存储文件总数
        downloader.file_total_nums.store(len, Ordering::Relaxed);
        // 设置失败文件数
        downloader.failed_files.lock().unwrap().clear();
        (resolved, len)
    } else {
        let finished_files = downloader.finished_files.lock().unwrap();
        (resolved.into_iter().filter(|item| !finished_files.contains(&item.filename)).collect(), downloader.file_total_nums.load(Ordering::Relaxed))
    };

    let tx1 = tx.clone();
    tx1.send(ChannelMessage::Downloading(file_total_nums as i32)).unwrap();

    let pool = ThreadPool::new(download_config.threads);
    for item in files {
        let downloader_clone = Arc::clone(&downloader);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(download_config.timeout))
            .user_agent("Chrome/147)")
            .build()
            .unwrap();
        let tx2 = tx.clone();

        pool.execute(move || {
            if downloader_clone.is_pause.load(Ordering::Relaxed) || downloader_clone.is_cancel.load(Ordering::Relaxed) {
                return;
            }

            let mut is_finish = false;
            // 带重试的下载
            for attempt in 0..download_config.retry {
                if let Ok(resp) = client.get(&item.download_url).send() && resp.status().is_success() {
                    let content = resp.bytes().unwrap();
                    let mut file = File::create(Path::new(&item.save_path).join(&item.filename)).unwrap();
                    file.write_all(&content).unwrap();
                    // 记录已下载的文件
                    let n = {
                        let mut downloaded = downloader_clone.finished_files.lock().unwrap();
                        downloaded.push(item.filename.to_owned());
                        downloaded.len()
                    };
                    tx2.send(ChannelMessage::Downloaded(n as i32)).unwrap();
                    is_finish = true;
                    break;
                }

                if attempt < download_config.retry - 1 {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            if !is_finish {
                // 记录下载失败的切片
                downloader_clone.failed_files.lock().unwrap().push(item.download_url.to_string());
                let failed_link_file = Path::new(&item.save_path).join("failed_link.txt");
                let mut file = OpenOptions::new().write(true).append(true).create(true).open(failed_link_file).unwrap();
                if file.metadata().unwrap().len() > 0 {
                    file.write_all(b"\n").unwrap();
                }
                file.write_all(item.download_url.as_bytes()).unwrap();
            }
        });
    }

    Ok(())
}

fn reset_download_status(
    ui_weak: &slint::Weak<AppWindow>,
    downloader: &Arc<Downloader>,
    message: SharedString,
    is_cancel_reset: bool,
) {
    let has_failed_file = if !is_cancel_reset {
        !downloader.failed_files.lock().unwrap().is_empty()
    } else {
        false
    };
    
    downloader.finished_files.lock().unwrap().clear();
    downloader.is_new_download.store(true, Ordering::Relaxed);

    ui_weak.upgrade_in_event_loop(move |ui| {
        ui.set_message(message);
        ui.set_enable_download_btn(true);
        ui.set_enable_pause_btn(false);
        ui.set_enable_cancel_btn(false);
        ui.set_is_downloading(false);
        ui.set_has_failed_file(has_failed_file);
        
        if is_cancel_reset {
            ui.set_total_nums(0);
            ui.set_downloaded_nums(0);
        }
    }).unwrap();
}