pub mod thread_pool;

use std::{fs::{self, File, OpenOptions}, io::{BufWriter, Write}, os::windows::process::CommandExt, path::Path, process::Command, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc}, thread, time::Duration};
use slint::SharedString;
use url::Url;
use reqwest::blocking::Client;
use crate::thread_pool::ThreadPool;

slint::include_modules!();

// 下载失败的文件名
const FAILED_FILENAME: &str = "failed_link.txt";

pub fn run() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;

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
            ui.set_message(SharedString::from("Parsing..."));

            tx_start.send(ChannelMessage::StartDownload(RequestData {
                save_directory: format!("{}\\{}", ui.get_work_dir(), ui.get_video_name()),
                m3u8_url: ui.get_m3u8_url().into(),
                user_agent: if ui.get_user_agent().trim().is_empty() { "Chrome/147.0".into() } else { ui.get_user_agent().into() },
                threads: ui.get_threads().parse::<usize>().unwrap_or(4),
                retry: ui.get_retry().parse::<u32>().unwrap_or(3),
                timeout: ui.get_retry().parse::<u64>().unwrap_or(5),
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
    window.on_open_failed_file({
        let ui_weak = window.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            Command::new("explorer").arg(Path::new(&format!("/select,{}\\{}\\{}", ui.get_work_dir(), ui.get_video_name(), FAILED_FILENAME))).spawn().unwrap().wait().unwrap();
        }
    });

    // 监控线程，避免阻塞UI
    let rx1 = Arc::new(Mutex::new(rx));
    let ui_weak = window.as_weak();
    thread::spawn(move || {
        loop_receive_message(tx.clone(), rx1, ui_weak);
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
    // 已下载文件数
    Downloaded(i32),
    RecycleTaskThead,
}

// 待下载文件信息
#[derive(Debug)]
struct WaitDownloadFile {
    // 切片名称
    slice_name: String,
    // 保存目录
    save_directory: String,
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
    save_directory: String,
    m3u8_url: String,
    user_agent: String,
    threads: usize,
    retry: u32,
    timeout: u64,
    is_convert: bool,
    is_delete_slice: bool,
}

// 启用新线程监控信道消息
fn loop_receive_message(
    tx: mpsc::Sender<ChannelMessage>,
    rx: Arc<Mutex<mpsc::Receiver<ChannelMessage>>>,
    ui_weak: slint::Weak<AppWindow>,
) {
    let download_task = Arc::new(DownloadTask::new());
    let task_thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    while let Ok(channel_message) = rx.lock().unwrap().recv() {
        match channel_message {
            // 启动下载任务
            ChannelMessage::StartDownload(request_data) => {
                let tx1 = tx.clone();
                let tx2 = tx.clone();
                let download_task1 = Arc::clone(&download_task);
                let ui_weak1 = ui_weak.clone();

                download_task1.is_pause.store(false, Ordering::Relaxed);
                download_task1.is_cancel.store(false, Ordering::Relaxed);
                download_task1.is_parse_fail.store(false, Ordering::Relaxed);
                download_task1.failed_files.lock().unwrap().clear();

                // 是否转MP4参数
                let (convert_args, is_delete_slice, save_directory) = if request_data.is_convert {
                    (vec!["-i".to_string(), format!("{}\\index.m3u8", request_data.save_directory), "-c".to_string(), "copy".to_string(), format!("{}\\output.mp4", request_data.save_directory)],
                    request_data.is_delete_slice, request_data.save_directory.to_string())
                } else {
                    (vec![], request_data.is_delete_slice, String::new())
                };

                let task_thread = thread::spawn(move || {
                    let ui_weak2 = ui_weak1.clone();
                    let download_task2 = Arc::clone(&download_task1);
                    
                    thread::spawn(move || {
                        let download_task3 = Arc::clone(&download_task2);
                        if let Err(e) = parse_download(download_task2, tx1, request_data) {
                            download_task3.is_parse_fail.store(true, Ordering::Relaxed);

                            let err_msg = e.to_string();
                            ui_weak2.upgrade_in_event_loop(move |ui| {
                                ui.set_message(err_msg.into());
                                ui.set_is_downloading(false);
                                ui.set_enable_start_btn(true);
                            }).unwrap();
                        }
                    }).join().unwrap();

                    // 通知准备回收本次任务线程
                    tx2.send(ChannelMessage::RecycleTaskThead).unwrap();

                    // 解析失败
                    if download_task1.is_parse_fail.load(Ordering::Relaxed) {
                        return;
                    }

                    // 暂停处理
                    if download_task1.is_pause.load(Ordering::Relaxed) {
                        ui_weak1.upgrade_in_event_loop(move |ui| {
                            ui.set_message("Paused.".into());
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

                    // 所有切片下载结束
                    if download_task1.file_total_nums.load(Ordering::Relaxed) == download_task1.downloaded_files.lock().unwrap().len() {
                        let mut msg = String::from("Download successful!");
                        // 不为空=转MP4
                        if !convert_args.is_empty() {
                            ui_weak1.upgrade_in_event_loop(move |ui| {
                                ui.set_message("Converting to mp4 now...".into());
                            }).unwrap();
                            if Command::new("ffmpeg").args(convert_args).creation_flags(0x08000000).output().unwrap().status.success() {
                                msg = String::from("Successfully converted to mp4");
                                // 是否删除切片
                                if is_delete_slice  {
                                    let downloaded_files = download_task1.downloaded_files.lock().unwrap();
                                    let mut delete_ok = true;
                                    for slice_name in downloaded_files.iter() {
                                        let file = format!("{}\\{}", save_directory, slice_name);
                                        let file = Path::new(&file);
                                        if file.is_file() && fs::remove_file(file).is_err() {
                                            msg.push_str(", but Failed to delete the slice.");
                                            delete_ok = false;
                                            break;
                                        }
                                    }
                                    if delete_ok {
                                        msg.push_str(" and delete slice.");
                                        // 同时删除M3U8
                                        let _ = fs::remove_file(format!("{}\\index.m3u8", save_directory));
                                    }
                                }
                            } else {
                                msg = String::from("Failed to converted to mp4.");
                            }
                        }
                        reset_download_status(&ui_weak1, &download_task1, SharedString::from(msg), false);
                    } else {
                        // 有切片下载失败了
                        reset_download_status(&ui_weak1, &download_task1, SharedString::new(), false);
                    }
                });
                *task_thread_handle.lock().unwrap() = Some(task_thread);
            },
            // 开始下载切片
            ChannelMessage::DownloadSlicing(file_total_nums) => {
                ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_message("Downloading...".into());
                    ui.set_total_nums(file_total_nums as i32);
                    ui.set_enable_pause_btn(true);
                    ui.set_enable_cancel_btn(true);
                }).unwrap();
            },
            // 某个切片下载完成
            ChannelMessage::Downloaded(downloaded_nums) => {
                ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_downloaded_nums(downloaded_nums);
                }).unwrap();
            },
            // 发出暂停下载信号
            ChannelMessage::Pause => {
                download_task.is_pause.store(true, Ordering::Relaxed);
                download_task.is_new_task.store(false, Ordering::Relaxed);
            },
            // 发出取消下载信号
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
                    // println!("release");
                }
            }
        }
    }
}

// 解析M3U8，并把待下载文件放入队列中
fn resolve_m3u8(request_data: RequestData) -> Result<Vec<WaitDownloadFile>, Box<dyn std::error::Error>> {
    // 创建视频目录
    let save_path = Path::new(&request_data.save_directory);
    if !save_path.is_dir() {
        fs::create_dir(save_path)?;
    }

    let base_url = Url::parse(&request_data.m3u8_url)?.join(".")?;
    let mut contents = String::new();
    let mut is_timeout = true;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(request_data.timeout))
        .danger_accept_invalid_certs(true)
        .user_agent(request_data.user_agent)
        .build()
        .unwrap();

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
        return Err("Connection timeout.".into());
    }

    if contents.trim().is_empty() {
        return Err("Content is empty.".into());
    }

    let m3u8_file = File::create(Path::new(&request_data.save_directory).join("index.m3u8"))?;
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
                save_directory: request_data.save_directory.to_owned(),
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
                    save_directory: request_data.save_directory.to_owned(),
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
        return Err("No downloadable.".into());
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

    // 若已存在下载失败文件，则清空内容
    let failed_filename = format!("{}\\{}", request_data.save_directory, FAILED_FILENAME);
    let failed_link_file = Path::new(&failed_filename);
    if failed_link_file.is_file() {
        fs::remove_file(failed_link_file).unwrap();
    }

    let pool = ThreadPool::new(request_data.threads);
    for item in wait_download_files {
        let download_task_clone = Arc::clone(&download_task);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(request_data.timeout))
            .danger_accept_invalid_certs(true)
            .user_agent(&request_data.user_agent)
            .build()
            .unwrap();
        let tx2 = tx.clone();

        pool.execute(move || {
            if download_task_clone.is_pause.load(Ordering::Relaxed) || download_task_clone.is_cancel.load(Ordering::Relaxed) {
                return;
            }

            let mut is_finish = false;
            // 带重试的下载
            for attempt in 0..request_data.retry {
                if let Ok(resp) = client.get(&item.download_url).send() && resp.status().is_success() {
                    let content = resp.bytes().unwrap();
                    let mut file = File::create(Path::new(&item.save_directory).join(&item.slice_name)).unwrap();
                    file.write_all(&content).unwrap();
                    // thread::sleep(Duration::from_millis(200));
                    // 记录已下载的文件
                    let n = {
                        let mut downloaded = download_task_clone.downloaded_files.lock().unwrap();
                        downloaded.push(item.slice_name.to_owned());
                        downloaded.len()
                    };
                    tx2.send(ChannelMessage::Downloaded(n as i32)).unwrap();
                    is_finish = true;
                    break;
                }

                if attempt < request_data.retry - 1 {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            if !is_finish {
                // 记录下载失败的切片
                download_task_clone.failed_files.lock().unwrap().push(item.download_url.to_string());
                let failed_link_file = Path::new(&item.save_directory).join(FAILED_FILENAME);
                let mut file = OpenOptions::new().append(true).create(true).open(failed_link_file).unwrap();
                if file.metadata().unwrap().len() > 0 {
                    file.write_all(b"\n").unwrap();
                }
                file.write_all(item.download_url.as_bytes()).unwrap();
            }
        });
    }

    Ok(())
}

// 取消或正常结束下载时（含下载失败的文件）重置UI
fn reset_download_status(
    ui_weak: &slint::Weak<AppWindow>,
    download_task: &Arc<DownloadTask>,
    message: SharedString,
    is_cancel_reset: bool,
) {
    let failed_file_nums = if !is_cancel_reset {
        download_task.failed_files.lock().unwrap().len()
    } else {
        0
    };

    let message: SharedString = if failed_file_nums > 0 { format!("The download is complete, but {} slice{} failed to download.", failed_file_nums, if failed_file_nums > 1 { "s" } else { "" }).into() } else { message };
    
    download_task.downloaded_files.lock().unwrap().clear();
    download_task.is_new_task.store(true, Ordering::Relaxed);
    download_task.file_total_nums.store(0, Ordering::Relaxed);

    ui_weak.upgrade_in_event_loop(move |ui| {
        ui.set_message(message);
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