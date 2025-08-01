use crate::stdout_is_a_pty;
use anyhow::{Context as _, Result};
use backtrace::{self, Backtrace};
use chrono::Utc;
use client::{TelemetrySettings, telemetry};
use db::kvp::KEY_VALUE_STORE;
use gpui::{App, AppContext as _, SemanticVersion};
use http_client::{self, HttpClient, HttpClientWithUrl, HttpRequestExt, Method};
use paths::{crashes_dir, crashes_retired_dir};
use project::Project;
use release_channel::{AppCommitSha, RELEASE_CHANNEL, ReleaseChannel};
use settings::Settings;
use smol::stream::StreamExt;
use std::{
    env,
    ffi::{OsStr, c_void},
    sync::{Arc, atomic::Ordering},
};
use std::{io::Write, panic, sync::atomic::AtomicU32, thread};
use telemetry_events::{LocationData, Panic, PanicRequest};
use url::Url;
use util::ResultExt;

static PANIC_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn init_panic_hook(
    app_version: SemanticVersion,
    app_commit_sha: Option<AppCommitSha>,
    system_id: Option<String>,
    installation_id: Option<String>,
    session_id: String,
) {
    let is_pty = stdout_is_a_pty();

    panic::set_hook(Box::new(move |info| {
        let prior_panic_count = PANIC_COUNT.fetch_add(1, Ordering::SeqCst);
        if prior_panic_count > 0 {
            // Give the panic-ing thread time to write the panic file
            loop {
                std::thread::yield_now();
            }
        }

        let thread = thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");

        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<Any>".to_string());

        if *release_channel::RELEASE_CHANNEL == ReleaseChannel::Dev {
            let location = info.location().unwrap();
            let backtrace = Backtrace::new();
            eprintln!(
                "Thread {:?} panicked with {:?} at {}:{}:{}\n{}{:?}",
                thread_name,
                payload,
                location.file(),
                location.line(),
                location.column(),
                match app_commit_sha.as_ref() {
                    Some(commit_sha) => format!(
                        "https://github.com/zed-industries/zed/blob/{}/{}#L{} \
                        (may not be uploaded, line may be incorrect if files modified)\n",
                        commit_sha.full(),
                        location.file(),
                        location.line()
                    ),
                    None => "".to_string(),
                },
                backtrace,
            );
            std::process::exit(-1);
        }
        let main_module_base_address = get_main_module_base_address();

        let backtrace = Backtrace::new();
        let mut symbols = backtrace
            .frames()
            .iter()
            .flat_map(|frame| {
                let base = frame
                    .module_base_address()
                    .unwrap_or(main_module_base_address);
                frame.symbols().iter().map(move |symbol| {
                    format!(
                        "{}+{}",
                        symbol
                            .name()
                            .as_ref()
                            .map_or("<unknown>".to_owned(), <_>::to_string),
                        (frame.ip() as isize).saturating_sub(base as isize)
                    )
                })
            })
            .collect::<Vec<_>>();

        // Strip out leading stack frames for rust panic-handling.
        if let Some(ix) = symbols
            .iter()
            .position(|name| name == "rust_begin_unwind" || name == "_rust_begin_unwind")
        {
            symbols.drain(0..=ix);
        }

        let panic_data = telemetry_events::Panic {
            thread: thread_name.into(),
            payload,
            location_data: info.location().map(|location| LocationData {
                file: location.file().into(),
                line: location.line(),
            }),
            app_version: app_version.to_string(),
            app_commit_sha: app_commit_sha.as_ref().map(|sha| sha.full()),
            release_channel: RELEASE_CHANNEL.dev_name().into(),
            target: env!("TARGET").to_owned().into(),
            os_name: telemetry::os_name(),
            os_version: Some(telemetry::os_version()),
            architecture: env::consts::ARCH.into(),
            panicked_on: Utc::now().timestamp_millis(),
            backtrace: symbols,
            system_id: system_id.clone(),
            installation_id: installation_id.clone(),
            session_id: session_id.clone(),
        };

        if let Some(panic_data_json) = serde_json::to_string_pretty(&panic_data).log_err() {
            log::error!("{}", panic_data_json);
        }
        zlog::flush();

        if !is_pty {
            if let Some(panic_data_json) = serde_json::to_string(&panic_data).log_err() {
                let timestamp = chrono::Utc::now().format("%Y_%m_%d %H_%M_%S").to_string();
                let panic_file_path = paths::logs_dir().join(format!("zed-{timestamp}.panic"));
                let panic_file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&panic_file_path)
                    .log_err();
                if let Some(mut panic_file) = panic_file {
                    writeln!(&mut panic_file, "{panic_data_json}").log_err();
                    panic_file.flush().log_err();
                }
            }
        }

        std::process::abort();
    }));
}

#[cfg(not(target_os = "windows"))]
fn get_main_module_base_address() -> *mut c_void {
    let mut dl_info = libc::Dl_info {
        dli_fname: std::ptr::null(),
        dli_fbase: std::ptr::null_mut(),
        dli_sname: std::ptr::null(),
        dli_saddr: std::ptr::null_mut(),
    };
    unsafe {
        libc::dladdr(get_main_module_base_address as _, &mut dl_info);
    }
    dl_info.dli_fbase
}

#[cfg(target_os = "windows")]
fn get_main_module_base_address() -> *mut c_void {
    std::ptr::null_mut()
}

pub fn init(
    http_client: Arc<HttpClientWithUrl>,
    system_id: Option<String>,
    installation_id: Option<String>,
    session_id: String,
    cx: &mut App,
) {
    #[cfg(target_os = "macos")]
    monitor_main_thread_hangs(http_client.clone(), installation_id.clone(), cx);

    let Some(panic_report_url) = http_client
        .build_zed_api_url("/telemetry/panics", &[])
        .log_err()
    else {
        return;
    };

    upload_panics_and_crashes(
        http_client.clone(),
        panic_report_url.clone(),
        installation_id.clone(),
        cx,
    );

    cx.observe_new(move |project: &mut Project, _, cx| {
        let http_client = http_client.clone();
        let panic_report_url = panic_report_url.clone();
        let session_id = session_id.clone();
        let installation_id = installation_id.clone();
        let system_id = system_id.clone();

        if let Some(ssh_client) = project.ssh_client() {
            ssh_client.update(cx, |client, cx| {
                if TelemetrySettings::get_global(cx).diagnostics {
                    let request = client.proto_client().request(proto::GetPanicFiles {});
                    cx.background_spawn(async move {
                        let panic_files = request.await?;
                        for file in panic_files.file_contents {
                            let panic: Option<Panic> = serde_json::from_str(&file)
                                .log_err()
                                .or_else(|| {
                                    file.lines()
                                        .next()
                                        .and_then(|line| serde_json::from_str(line).ok())
                                })
                                .unwrap_or_else(|| {
                                    log::error!("failed to deserialize panic file {:?}", file);
                                    None
                                });

                            if let Some(mut panic) = panic {
                                panic.session_id = session_id.clone();
                                panic.system_id = system_id.clone();
                                panic.installation_id = installation_id.clone();

                                upload_panic(&http_client, &panic_report_url, panic, &mut None)
                                    .await?;
                            }
                        }

                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
            })
        }
    })
    .detach();
}

#[cfg(target_os = "macos")]
pub fn monitor_main_thread_hangs(
    http_client: Arc<HttpClientWithUrl>,
    installation_id: Option<String>,
    cx: &App,
) {
    // This is too noisy to ship to stable for now.
    if !matches!(
        ReleaseChannel::global(cx),
        ReleaseChannel::Dev | ReleaseChannel::Nightly | ReleaseChannel::Preview
    ) {
        return;
    }

    use nix::sys::signal::{
        SaFlags, SigAction, SigHandler, SigSet,
        Signal::{self, SIGUSR2},
        sigaction,
    };

    use parking_lot::Mutex;

    use http_client::Method;
    use std::{
        ffi::c_int,
        sync::{OnceLock, mpsc},
        time::Duration,
    };
    use telemetry_events::{BacktraceFrame, HangReport};

    use nix::sys::pthread;

    let foreground_executor = cx.foreground_executor();
    let background_executor = cx.background_executor();
    let telemetry_settings = *client::TelemetrySettings::get_global(cx);

    // Initialize SIGUSR2 handler to send a backtrace to a channel.
    let (backtrace_tx, backtrace_rx) = mpsc::channel();
    static BACKTRACE: Mutex<Vec<backtrace::Frame>> = Mutex::new(Vec::new());
    static BACKTRACE_SENDER: OnceLock<mpsc::Sender<()>> = OnceLock::new();
    BACKTRACE_SENDER.get_or_init(|| backtrace_tx);
    BACKTRACE.lock().reserve(100);

    fn handle_backtrace_signal() {
        unsafe {
            extern "C" fn handle_sigusr2(_i: c_int) {
                unsafe {
                    // ASYNC SIGNAL SAFETY: This lock is only accessed one other time,
                    // which can only be triggered by This signal handler. In addition,
                    // this signal handler is immediately removed by SA_RESETHAND, and this
                    // signal handler cannot be re-entrant due to the SIGUSR2 mask defined
                    // below
                    let mut bt = BACKTRACE.lock();
                    bt.clear();
                    backtrace::trace_unsynchronized(|frame| {
                        if bt.len() < bt.capacity() {
                            bt.push(frame.clone());
                            true
                        } else {
                            false
                        }
                    });
                }

                BACKTRACE_SENDER.get().unwrap().send(()).ok();
            }

            let mut mask = SigSet::empty();
            mask.add(SIGUSR2);
            sigaction(
                Signal::SIGUSR2,
                &SigAction::new(
                    SigHandler::Handler(handle_sigusr2),
                    SaFlags::SA_RESTART | SaFlags::SA_RESETHAND,
                    mask,
                ),
            )
            .log_err();
        }
    }

    handle_backtrace_signal();
    let main_thread = pthread::pthread_self();

    let (mut tx, mut rx) = futures::channel::mpsc::channel(3);
    foreground_executor
        .spawn(async move { while (rx.next().await).is_some() {} })
        .detach();

    background_executor
        .spawn({
            let background_executor = background_executor.clone();
            async move {
                loop {
                    background_executor.timer(Duration::from_secs(1)).await;
                    match tx.try_send(()) {
                        Ok(_) => continue,
                        Err(e) => {
                            if e.into_send_error().is_full() {
                                pthread::pthread_kill(main_thread, SIGUSR2).log_err();
                            }
                            // Only detect the first hang
                            break;
                        }
                    }
                }
            }
        })
        .detach();

    let app_version = release_channel::AppVersion::global(cx);
    let os_name = client::telemetry::os_name();

    background_executor
        .clone()
        .spawn(async move {
            let os_version = client::telemetry::os_version();

            loop {
                while backtrace_rx.recv().is_ok() {
                    if !telemetry_settings.diagnostics {
                        return;
                    }

                    // ASYNC SIGNAL SAFETY: This lock is only accessed _after_
                    // the backtrace transmitter has fired, which itself is only done
                    // by the signal handler. And due to SA_RESETHAND  the signal handler
                    // will not run again until `handle_backtrace_signal` is called.
                    let raw_backtrace = BACKTRACE.lock().drain(..).collect::<Vec<_>>();
                    let backtrace: Vec<_> = raw_backtrace
                        .into_iter()
                        .map(|frame| {
                            let mut btf = BacktraceFrame {
                                ip: frame.ip() as usize,
                                symbol_addr: frame.symbol_address() as usize,
                                base: frame.module_base_address().map(|addr| addr as usize),
                                symbols: vec![],
                            };

                            backtrace::resolve_frame(&frame, |symbol| {
                                if let Some(name) = symbol.name() {
                                    btf.symbols.push(name.to_string());
                                }
                            });

                            btf
                        })
                        .collect();

                    // IMPORTANT: Don't move this to before `BACKTRACE.lock()`
                    handle_backtrace_signal();

                    log::error!(
                        "Suspected hang on main thread:\n{}",
                        backtrace
                            .iter()
                            .flat_map(|bt| bt.symbols.first().as_ref().map(|s| s.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );

                    let report = HangReport {
                        backtrace,
                        app_version: Some(app_version),
                        os_name: os_name.clone(),
                        os_version: Some(os_version.clone()),
                        architecture: env::consts::ARCH.into(),
                        installation_id: installation_id.clone(),
                    };

                    let Some(json_bytes) = serde_json::to_vec(&report).log_err() else {
                        continue;
                    };

                    let Some(checksum) = client::telemetry::calculate_json_checksum(&json_bytes)
                    else {
                        continue;
                    };

                    let Ok(url) = http_client.build_zed_api_url("/telemetry/hangs", &[]) else {
                        continue;
                    };

                    let Ok(request) = http_client::Request::builder()
                        .method(Method::POST)
                        .uri(url.as_ref())
                        .header("x-zed-checksum", checksum)
                        .body(json_bytes.into())
                    else {
                        continue;
                    };

                    if let Some(response) = http_client.send(request).await.log_err() {
                        if response.status() != 200 {
                            log::error!("Failed to send hang report: HTTP {:?}", response.status());
                        }
                    }
                }
            }
        })
        .detach()
}

fn upload_panics_and_crashes(
    http: Arc<HttpClientWithUrl>,
    panic_report_url: Url,
    installation_id: Option<String>,
    cx: &App,
) {
    let telemetry_settings = *client::TelemetrySettings::get_global(cx);
    cx.background_spawn(async move {
        let most_recent_panic =
            upload_previous_panics(http.clone(), &panic_report_url, telemetry_settings)
                .await
                .log_err()
                .flatten();
        upload_previous_crashes(http, most_recent_panic, installation_id, telemetry_settings)
            .await
            .log_err()
    })
    .detach()
}

/// Uploads panics via `zed.dev`.
async fn upload_previous_panics(
    http: Arc<HttpClientWithUrl>,
    panic_report_url: &Url,
    telemetry_settings: client::TelemetrySettings,
) -> anyhow::Result<Option<(i64, String)>> {
    let mut children = smol::fs::read_dir(paths::logs_dir()).await?;

    let mut most_recent_panic = None;

    while let Some(child) = children.next().await {
        let child = child?;
        let child_path = child.path();

        if child_path.extension() != Some(OsStr::new("panic")) {
            continue;
        }
        let filename = if let Some(filename) = child_path.file_name() {
            filename.to_string_lossy()
        } else {
            continue;
        };

        if !filename.starts_with("zed") {
            continue;
        }

        if telemetry_settings.diagnostics {
            let panic_file_content = smol::fs::read_to_string(&child_path)
                .await
                .context("error reading panic file")?;

            let panic: Option<Panic> = serde_json::from_str(&panic_file_content)
                .log_err()
                .or_else(|| {
                    panic_file_content
                        .lines()
                        .next()
                        .and_then(|line| serde_json::from_str(line).ok())
                })
                .unwrap_or_else(|| {
                    log::error!("failed to deserialize panic file {:?}", panic_file_content);
                    None
                });

            if let Some(panic) = panic {
                if !upload_panic(&http, &panic_report_url, panic, &mut most_recent_panic).await? {
                    continue;
                }
            }
        }

        // We've done what we can, delete the file
        std::fs::remove_file(child_path)
            .context("error removing panic")
            .log_err();
    }
    Ok(most_recent_panic)
}

async fn upload_panic(
    http: &Arc<HttpClientWithUrl>,
    panic_report_url: &Url,
    panic: telemetry_events::Panic,
    most_recent_panic: &mut Option<(i64, String)>,
) -> Result<bool> {
    *most_recent_panic = Some((panic.panicked_on, panic.payload.clone()));

    let json_bytes = serde_json::to_vec(&PanicRequest { panic }).unwrap();

    let Some(checksum) = client::telemetry::calculate_json_checksum(&json_bytes) else {
        return Ok(false);
    };

    let Ok(request) = http_client::Request::builder()
        .method(Method::POST)
        .uri(panic_report_url.as_ref())
        .header("x-zed-checksum", checksum)
        .body(json_bytes.into())
    else {
        return Ok(false);
    };

    let response = http.send(request).await.context("error sending panic")?;
    if !response.status().is_success() {
        log::error!("Error uploading panic to server: {}", response.status());
    }

    Ok(true)
}
const LAST_CRASH_UPLOADED: &str = "LAST_CRASH_UPLOADED";

/// upload crashes from apple's diagnostic reports to our server.
/// (only if telemetry is enabled)
async fn upload_previous_crashes(
    http: Arc<HttpClientWithUrl>,
    most_recent_panic: Option<(i64, String)>,
    installation_id: Option<String>,
    telemetry_settings: client::TelemetrySettings,
) -> Result<()> {
    if !telemetry_settings.diagnostics {
        return Ok(());
    }
    let last_uploaded = KEY_VALUE_STORE
        .read_kvp(LAST_CRASH_UPLOADED)?
        .unwrap_or("zed-2024-01-17-221900.ips".to_string()); // don't upload old crash reports from before we had this.
    let mut uploaded = last_uploaded.clone();

    let crash_report_url = http.build_zed_api_url("/telemetry/crashes", &[])?;

    // Crash directories are only set on macOS.
    for dir in [crashes_dir(), crashes_retired_dir()]
        .iter()
        .filter_map(|d| d.as_deref())
    {
        let mut children = smol::fs::read_dir(&dir).await?;
        while let Some(child) = children.next().await {
            let child = child?;
            let Some(filename) = child
                .path()
                .file_name()
                .map(|f| f.to_string_lossy().to_lowercase())
            else {
                continue;
            };

            if !filename.starts_with("zed-") || !filename.ends_with(".ips") {
                continue;
            }

            if filename <= last_uploaded {
                continue;
            }

            let body = smol::fs::read_to_string(&child.path())
                .await
                .context("error reading crash file")?;

            let mut request = http_client::Request::post(&crash_report_url.to_string())
                .follow_redirects(http_client::RedirectPolicy::FollowAll)
                .header("Content-Type", "text/plain");

            if let Some((panicked_on, payload)) = most_recent_panic.as_ref() {
                request = request
                    .header("x-zed-panicked-on", format!("{panicked_on}"))
                    .header("x-zed-panic", payload)
            }
            if let Some(installation_id) = installation_id.as_ref() {
                request = request.header("x-zed-installation-id", installation_id);
            }

            let request = request.body(body.into())?;

            let response = http.send(request).await.context("error sending crash")?;
            if !response.status().is_success() {
                log::error!("Error uploading crash to server: {}", response.status());
            }

            if uploaded < filename {
                uploaded.clone_from(&filename);
                KEY_VALUE_STORE
                    .write_kvp(LAST_CRASH_UPLOADED.to_string(), filename)
                    .await?;
            }
        }
    }

    Ok(())
}
