pub mod thread_pool;

use std::{fs::{self, File, OpenOptions}, io::{self, BufWriter, Write}, os::windows::process::CommandExt, path::{Path, PathBuf}, process::Command, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc}, thread, time::Duration};
use slint::{PhysicalPosition, SharedString};
use url::Url;
use reqwest::blocking::Client;
use crate::thread_pool::ThreadPool;
use regex::Regex;
use winsafe::{GetSystemMetrics, co::SM};

slint::include_modules!();

// 下载失败的文件名
const FAILED_FILENAME: &str = "failed_link.txt";
// M3U8文件名
const M3U8_FILENAME: &str = "index.m3u8";
// user agent
const APP_USER_AGENT: &str = "Chrome/147";

pub fn run() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;

    // 设置窗口居中
    let x = (GetSystemMetrics(SM::CXSCREEN) - 370) / 2;
    let y = (GetSystemMetrics(SM::CYSCREEN) - 600) / 2; // 尽量偏高
    window.window().set_position(slint::WindowPosition::Physical(PhysicalPosition {x: x, y: y}));

    let (tx, rx) = mpsc::channel();

    // 启动新的下载任务
    window.on_start_download({
        let ui_weak = window.as_weak();
        let tx_start = tx.clone();
        move || {
            let ui = ui_weak.unwrap();

            ui.set_enable_start_btn(false);
            ui.set_is_downloading(true);
            ui.set_has_failed_file(false);
            ui.invoke_show_message(SharedString::from("Parsing..."), false);

            tx_start.send(ChannelMessage::StartDownload(RequestData {
                save_path: create_safe_save_path(&ui.get_work_dir(), &ui.get_video_name()).unwrap(),
                m3u8_url: ui.get_m3u8_url().into(),
                threads: ui.get_threads().parse::<usize>().unwrap_or(4),
                retry: ui.get_retry().parse::<u32>().unwrap_or(3),
                timeout: ui.get_retry().parse::<u64>().unwrap_or(3),
                is_convert: ui.get_is_convert(),
                is_delete_segment: ui.get_is_delete_segment(),
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
            ui.invoke_show_message(SharedString::from("Pausing..."), false);

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
            ui.set_enable_pause_btn(false);
            ui.invoke_show_message(SharedString::from("Canceling..."), false);
            
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
    window.on_open_failed_file({
        let ui_weak = window.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            Command::new("explorer")
                .arg(Path::new("/select,")
                    .join(ui.get_work_dir())
                    .join(ui.get_video_name())
                    .join(FAILED_FILENAME))
                .spawn()
                .unwrap()
                .wait()
                .unwrap();
        }
    });

    // 监控线程，避免阻塞UI
    let ui_weak = window.as_weak();
    thread::spawn(move || {
        loop_receive_message(tx.clone(), rx, ui_weak);
    });

    window.run()
}

// 枚举通道消息
#[derive(Debug)]
enum ChannelMessage {
    StartDownload(RequestData),
    DownloadSlicing(usize),
    Pause,
    Cancel,
    RecycleTaskThead,
    // 切片下载完成
    SliceCompleted {
        slice_name: String,
        content_length: u64,
    },
}

// 待下载文件信息
#[derive(Debug)]
struct WaitDownloadFile {
    // 切片名称
    slice_name: String,
    // 保存目录
    save_path: PathBuf,
    // 下载链接
    download_url: String,
}

// 下载管理
struct DownloadTask {
    is_new_task: AtomicBool,
    is_pause: AtomicBool,
    is_cancel: AtomicBool,
    // 是否解析M3U8失败
    is_parse_fail: AtomicBool,
    file_total_nums: AtomicUsize,
    // 已下载完成的文件列表，存储文件名
    downloaded_files: Mutex<Vec<String>>,
    // 下载失败的链接，存储url
    failed_files: Mutex<Vec<String>>,
}

impl DownloadTask {
    fn new() -> Self {
        Self {
            is_new_task: AtomicBool::new(true),
            is_pause: AtomicBool::new(false),
            is_cancel: AtomicBool::new(false),
            is_parse_fail: AtomicBool::new(false),
            file_total_nums: AtomicUsize::new(0),
            downloaded_files: Mutex::new(vec![]),
            failed_files: Mutex::new(vec![]),
        }
    }
}

// 请求参数
#[derive(Debug, Clone)]
struct RequestData {
    save_path: PathBuf,
    m3u8_url: String,
    threads: usize,
    retry: u32,
    timeout: u64,
    is_convert: bool,
    is_delete_segment: bool,
}

// 启用新线程监控信道消息
fn loop_receive_message(
    tx: mpsc::Sender<ChannelMessage>,
    rx: mpsc::Receiver<ChannelMessage>,
    ui_weak: slint::Weak<AppWindow>,
) {
    // 新下载任务配置
    let download_task = Arc::new(DownloadTask::new());
    // 收集新任务线程，下载完成后释放
    let task_thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    // 当前已下载的文件大小
    let mut current_content_length: u64 = 0;

    // 一个线程、一个接收者时使用while let
    while let Ok(channel_message) = rx.recv() {
        match channel_message {
            // 启动下载
            ChannelMessage::StartDownload(request_data) => {
                let tx1 = tx.clone();
                let tx2 = tx.clone();
                let download_task1 = Arc::clone(&download_task);
                let ui_weak1 = ui_weak.clone();

                download_task1.is_pause.store(false, Ordering::Relaxed);
                download_task1.is_cancel.store(false, Ordering::Relaxed);
                download_task1.is_parse_fail.store(false, Ordering::Relaxed);
                download_task1.failed_files.lock().unwrap().clear();

                if download_task1.is_new_task.load(Ordering::Relaxed) {
                    current_content_length = 0;
                }

                // 构建转换MP4参数
                let (convert_args, is_delete_segment, save_path) = if request_data.is_convert {
                    (vec![
                        "-allowed_extensions".to_string(),
                        "ALL".to_string(),
                        "-i".to_string(),
                        request_data.save_path.join(M3U8_FILENAME).to_str().unwrap().to_string(),
                        "-c".to_string(),
                        "copy".to_string(),
                        request_data.save_path.join("output.mp4").to_str().unwrap().to_string()
                    ], request_data.is_delete_segment, request_data.save_path.clone())
                } else {
                    (vec![], false, PathBuf::new())
                };

                // 任务线程
                let task_thread = thread::spawn(move || {
                    let ui_weak2 = ui_weak1.clone();
                    let download_task2 = Arc::clone(&download_task1);
                    // 解析并下载的线程
                    let download_handle = thread::spawn(move || {
                        let download_task3 = Arc::clone(&download_task2);
                        // 解析失败
                        if let Err(e) = parse_download(download_task2, tx1, request_data) {
                            download_task3.is_parse_fail.store(true, Ordering::Relaxed);
                            let err_msg = e.to_string();
                            ui_weak2.upgrade_in_event_loop(move |ui| {
                                ui.invoke_show_message(err_msg.into(), true);
                                ui.set_is_downloading(false);
                                ui.set_enable_start_btn(true);
                            }).unwrap();
                        }
                    });
                    // 等待完成，可记录下载耗时
                    download_handle.join().unwrap();
                    // 通知准备回收本次任务线程
                    tx2.send(ChannelMessage::RecycleTaskThead).unwrap();

                    // 解析失败
                    if download_task1.is_parse_fail.load(Ordering::Relaxed) {
                        return;
                    }

                    // 暂停处理
                    if download_task1.is_pause.load(Ordering::Relaxed) {
                        ui_weak1.upgrade_in_event_loop(move |ui| {
                            ui.invoke_show_message("Paused.".into(), false);
                            ui.set_is_pause(true);
                            ui.set_enable_start_btn(true);
                        }).unwrap();
                        return;
                    }

                    // 取消处理
                    if download_task1.is_cancel.load(Ordering::Relaxed) {
                        reset_download_status(&ui_weak1, &download_task1, SharedString::from("Canceled."), true);
                        return;
                    }

                    // 已下载的文件
                    let downloaded_files = {
                        let guard = download_task1.downloaded_files.lock().unwrap();
                        guard.clone()
                    };
                    // 所有切片均已下载
                    if download_task1.file_total_nums.load(Ordering::Relaxed) == downloaded_files.len() {
                        let mut message = String::from("All segments have been downloaded!");
                        // 不为空 == 转MP4
                        if !convert_args.is_empty() {
                            ui_weak1.upgrade_in_event_loop(move |ui| {
                                ui.invoke_show_message("Converting to MP4 now...".into(), false);
                            }).unwrap();
                            // 使用ffmpeg转MP4
                            match convert_and_delete(convert_args, is_delete_segment, downloaded_files, save_path) {
                                Ok(msg) => {
                                    message = msg;
                                },
                                Err(e) => {
                                    match e.kind() {
                                        io::ErrorKind::NotFound => {
                                            message = String::from("An error occurred: Cannot find the ffmpeg command");
                                        },
                                        _ => {
                                           message = String::from(e.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        reset_download_status(&ui_weak1, &download_task1, SharedString::from(message), false);
                    } else {
                        // 有切片下载失败了
                        reset_download_status(&ui_weak1, &download_task1, SharedString::new(), false);
                    }
                });
                // 收集任务线程句柄
                *task_thread_handle.lock().unwrap() = Some(task_thread);
            },
            // M3U8解析成功，即将下载切片
            ChannelMessage::DownloadSlicing(file_total_nums) => {
                ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.invoke_show_message("Downloading...".into(), false);
                    ui.set_total_nums(file_total_nums as i32);
                    ui.set_enable_pause_btn(true);
                    ui.set_enable_cancel_btn(true);
                    ui.set_is_pause(false);
                }).unwrap();
            },
            // 切片下载完成，更新进度
            ChannelMessage::SliceCompleted { slice_name, content_length } => {
                let n = {
                    let mut downloaded = download_task.downloaded_files.lock().unwrap();
                    downloaded.push(slice_name);
                    downloaded.len()
                } as i32;
                current_content_length += content_length;
                ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_downloaded_nums(n);
                    ui.set_content_length(byte_convert(current_content_length).into());
                }).unwrap();
            },
            // 响应暂停信号
            ChannelMessage::Pause => {
                download_task.is_pause.store(true, Ordering::Relaxed);
                download_task.is_new_task.store(false, Ordering::Relaxed);
            },
            // 响应取消信号
            ChannelMessage::Cancel => {
                download_task.is_cancel.store(true, Ordering::Relaxed);
                if download_task.is_pause.load(Ordering::Relaxed) {
                    reset_download_status(&ui_weak, &download_task, SharedString::from("Canceled."), true);
                }
            },
            // 回收任务线程
            ChannelMessage::RecycleTaskThead => {
                if let Some(handle) = task_thread_handle.lock().unwrap().take() {
                    handle.join().unwrap();
                    println!("release task thread");
                }
            }
        }
    }
}

// 转换MP4并删除切片
fn convert_and_delete(
    args: Vec<String>,
    is_delete_segment: bool,
    downloaded_files: Vec<String>,
    save_path: PathBuf,
) -> Result<String, io::Error> {
    let cmd = Command::new("ffmpeg").args(args).creation_flags(0x08000000).output()?;
    // 转换MP4成功
    if cmd.status.success() {
        let mut msg = String::from("Successfully converted to MP4!");
        // 勾选了要删除切片
        if is_delete_segment {
            // 删除m3u8
            let _ = fs::remove_file(save_path.join(M3U8_FILENAME));
            // 删除切片
            for slice_name in downloaded_files {
                let slice_filepath = save_path.join(slice_name);
                if slice_filepath.is_file() {
                    let _ = fs::remove_file(slice_filepath);
                }
            }
            msg.push_str(" And deleted the segments!");
        }
        Ok(msg.into())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, "Failed to convert MP4"))
    }
}

// 解析M3U8，并把待下载文件放入队列中
fn resolve_m3u8(request_data: RequestData) -> Result<Vec<WaitDownloadFile>, Box<dyn std::error::Error>> {
    let base_url = Url::parse(&request_data.m3u8_url)?.join(".")?;
    let mut contents = String::new();
    let mut is_timeout = true;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(request_data.timeout))
        .timeout(Duration::from_secs(100))
        .user_agent(APP_USER_AGENT)
        .build()?;

    for attemp in 0..request_data.retry {
        if let Ok(res) = client.get(&request_data.m3u8_url).send() && res.status().is_success() {
            contents = res.text()?;
            is_timeout = false;
            break;
        }

        if attemp < request_data.retry - 1 {
            thread::sleep(Duration::from_millis(200));
        }
    }

    if is_timeout {
        return Err("Connection timeout".into());
    }

    if contents.trim().is_empty() {
        return Err("Content is empty".into());
    }

    let m3u8_file = File::create(request_data.save_path.join(M3U8_FILENAME))?;
    let mut writer = BufWriter::new(&m3u8_file);
    let mut wait_download_files: Vec<WaitDownloadFile> = vec![];

    for line in contents.lines() {
        // :todo内部还有M3U8
        if line.ends_with(".m3u8") {
            break;
        }

        if line.starts_with("#EXT-X-KEY") {
            let key = line.split("URI=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
            let (key_name, url) = parse_m3u8_key_segment(&base_url, key)?;
            wait_download_files.push(WaitDownloadFile {
                slice_name: key_name.to_owned(),
                save_path: request_data.save_path.to_owned(),
                download_url: url,
            });
            writeln!(writer, "{}", line.replace(key, &key_name))?;
            continue;
        }
        
        if !line.starts_with("#") && !line.trim().is_empty() {
            let (slice_name, url) = parse_m3u8_key_segment(&base_url, line)?;
            if !slice_name.trim().is_empty() {
                wait_download_files.push(WaitDownloadFile{
                    slice_name: slice_name.to_owned(),
                    save_path: request_data.save_path.to_owned(),
                    download_url: url,
                });
                writeln!(writer, "{}", slice_name.to_owned())?;
            }
            continue;
        }
        
        if !line.trim().is_empty() {
            writeln!(writer, "{}", line)?;
        }
    }

    if wait_download_files.is_empty() {
        return Err("No downloadable".into());
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
        .ok_or_else(|| format!("no slice:{}", url))? 
        .to_string();

    Ok((filename, url.to_string()))
}

// 解析并下载
fn parse_download(
    download_task: Arc<DownloadTask>,
    tx: mpsc::Sender<ChannelMessage>,
    request_data: RequestData,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_files = resolve_m3u8(request_data.clone())?;
    let file_total_nums = all_files.len();
    let wait_download_files = if download_task.is_new_task.load(Ordering::Relaxed) {
        // 存储总切片数
        download_task.file_total_nums.store(all_files.len(), Ordering::Relaxed);
        all_files
    } else {
        let downloaded_files = download_task.downloaded_files.lock().unwrap();
        all_files.into_iter().filter(|item| !downloaded_files.contains(&item.slice_name)).collect()
    };

    let tx1 = tx.clone();
    tx1.send(ChannelMessage::DownloadSlicing(file_total_nums)).unwrap();

    // 处理存储下载失败的文件
    let failed_filepath  = request_data.save_path.join(FAILED_FILENAME);
    if failed_filepath.is_file() {
        fs::remove_file(failed_filepath).unwrap();
    }

    let pool = ThreadPool::new(request_data.threads);
    for item in wait_download_files {
        let download_task_clone = Arc::clone(&download_task);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(request_data.timeout))
            .timeout(Duration::from_secs(100))
            .user_agent(APP_USER_AGENT)
            .build()?;
        let tx2 = tx.clone();

        pool.execute(move || {
            if download_task_clone.is_pause.load(Ordering::Relaxed) || download_task_clone.is_cancel.load(Ordering::Relaxed) {
                return;
            }
            let mut is_finish = false;
            for attempt in 0..request_data.retry {
                if let Ok(resp) = client.get(&item.download_url).send() && resp.status().is_success() {
                    let content_length = resp.content_length().unwrap_or(0);
                    if let Ok(bytes) = resp.bytes() {
                        let mut file = File::create(item.save_path.join(&item.slice_name)).unwrap();
                        file.write_all(&bytes).unwrap();
                        let _ = tx2.send(ChannelMessage::SliceCompleted { slice_name: item.slice_name, content_length });
                        is_finish = true;
                        break;
                    }
                }

                // 重试
                if attempt < request_data.retry - 1 {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            // 切片下载失败
            if !is_finish {
                record_failed_file(&download_task_clone, item.save_path, &item.download_url);
            }
        });
    }

    Ok(())
}

fn byte_convert(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }

    if size == 1024 {
        return String::from("1 KB");
    }

    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = size as f64;
    let mut unit_index = 0;
    
    while size > 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    format!("{:.2} {}", size, UNITS[unit_index])
}

// 记录下载失败的切片
fn record_failed_file(
    download_manager: &Arc<DownloadTask>,
    save_path: PathBuf,
    download_url: &str,
) {
    download_manager.failed_files.lock().unwrap().push(download_url.to_string());
    let failed_link_file = save_path.join(FAILED_FILENAME);
    let mut file = OpenOptions::new().append(true).create(true).open(failed_link_file).unwrap();
    if file.metadata().unwrap().len() > 0 {
        file.write_all(b"\n").unwrap();
    }
    file.write_all(download_url.as_bytes()).unwrap();
}

// 取消或正常结束下载时（含下载失败的文件）重置UI
fn reset_download_status(
    ui_weak: &slint::Weak<AppWindow>,
    download_task: &Arc<DownloadTask>,
    message: SharedString,
    is_cancel_reset: bool,
) {
    // 非取消下载时计算失败文件数
    let failed_file_nums = if !is_cancel_reset {
        download_task.failed_files.lock().unwrap().len()
    } else {
        0
    };

    // 构建最终消息
    let final_message = if failed_file_nums > 0 {
        format!(
            "{} file{} cannot be downloaded",
            failed_file_nums,
            if failed_file_nums > 1 { "s" } else { "" }
        ).into()
    } else {
        message
    };
    
    // 快速释放该锁
    {
        download_task.downloaded_files.lock().unwrap().clear();
    }

    download_task.is_new_task.store(true, Ordering::Relaxed);
    download_task.file_total_nums.store(0, Ordering::Relaxed);

    ui_weak.upgrade_in_event_loop(move |ui| {
        ui.invoke_show_message(final_message, failed_file_nums > 0);
        ui.set_enable_start_btn(true);
        ui.set_enable_pause_btn(false);
        ui.set_enable_cancel_btn(false);
        ui.set_is_downloading(false);
        ui.set_is_pause(false);
        ui.set_has_failed_file(failed_file_nums > 0);
        
        if is_cancel_reset {
            ui.set_total_nums(0);
            ui.set_downloaded_nums(0);
        }
    }).unwrap();
}

// 安全的创建保存目录
fn create_safe_save_path(work_dir: &SharedString, video_name: &SharedString) -> Option<PathBuf> {
    if video_name.is_empty() {
        return None;
    }

    // 只取文件名部分（去除路径）
    let safe_name = Path::new(video_name).file_name().and_then(|n| n.to_str()).unwrap_or(video_name);

    // 移除非法字符
    let re = Regex::new(r#"[<>:"/\\|?*]"#).unwrap();
    let clean_name = re.replace_all(safe_name, "_");
    // 中文下最多50个字符
    let clean_name = if clean_name.len() > 150 {
        &clean_name[..150]
    } else {
        &clean_name
    };

    // 创建保存目录
    let save_path = Path::new(work_dir).join(clean_name);
    if !save_path.is_dir() {
        fs::create_dir(&save_path).unwrap();
    }

    Some(save_path)
}