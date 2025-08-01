// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod mugi_schema;
mod obs;
mod udp;
mod vlc_manager;

use log::{debug, error, info};
use mugi_schema::MugiCmd;
use std::sync::{Arc, Mutex, RwLock};
use tauri::AppHandle;
use tauri_plugin_log::{Target, TargetKind};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::mpsc::{self};
use udp::bind_socket;
use vlc_manager::VlcManager;

// 複雑な型を簡素化するためのtype alias
type ObsConnectionInfo = Arc<Mutex<Option<(String, u16, Option<String>)>>>;

// グローバル状態管理用の構造体
struct AppState {
    obs_connection_info: ObsConnectionInfo,
    is_system_running: Arc<Mutex<bool>>,
    sleep_duration_sec: Arc<RwLock<u64>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            obs_connection_info: Arc::new(Mutex::new(None)),
            is_system_running: Arc::new(Mutex::new(false)),
            sleep_duration_sec: Arc::new(RwLock::new(3)), // デフォルト3秒
        }
    }
}

#[tauri::command]
async fn get_sleep_duration(state: tauri::State<'_, AppState>) -> Result<u64, String> {
    let sleep_dur = state.sleep_duration_sec.read().unwrap();
    Ok(*sleep_dur)
}

#[tauri::command]
async fn set_sleep_duration(
    duration: u64,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let clamped_duration = duration.max(1).min(30); // 1-30秒の範囲制限

    {
        let mut sleep_dur = state.sleep_duration_sec.write().unwrap();
        *sleep_dur = clamped_duration;
    }

    Ok(format!(
        "録画遅延時間を{}秒に設定しました",
        clamped_duration
    ))
}

#[tauri::command]
async fn play_highlights(
    video_paths: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    if video_paths.is_empty() {
        return Ok("再生する動画がありません".to_string());
    }

    // OBS接続情報を取得
    let (host, port, password) = {
        let conn_info = state.obs_connection_info.lock().unwrap();
        match conn_info.as_ref() {
            Some((host, port, password)) => (host.clone(), *port, password.clone()),
            None => return Err("OBS接続情報が見つかりません".to_string()),
        }
    };

    // OBS接続を作成
    let mut obs = obs::Obs::new();
    let password_ref = password.as_deref();
    obs.connect(&host, port, password_ref)
        .await
        .map_err(|e| format!("Failed to connect to OBS: {}", e))?;

    // ファイル名からPathBufに変換（仮想的なパスとして扱う）
    let movie_pathes: Vec<std::path::PathBuf> =
        video_paths.iter().map(std::path::PathBuf::from).collect();

    // VLCソースで動画再生
    if let Err(e) = obs.play_vlc_source(&movie_pathes).await {
        return Err(format!("Failed to play VLC source: {}", e));
    }

    Ok(format!(
        "{}個のハイライト動画を再生しました",
        video_paths.len()
    ))
}

#[tauri::command]
async fn connect_obs(
    host: String,
    port: u16,
    password: Option<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<String, String> {
    info!("Attempting to connect to OBS at {}:{}", host, port);

    // 既にシステムが動作中の場合はエラー
    {
        let is_running = state.is_system_running.lock().unwrap();
        if *is_running {
            return Err("システムは既に動作中です".to_string());
        }
    }

    let mut obs = obs::Obs::new();
    let password_ref = password.as_deref();

    // OBS接続試行
    match obs.connect(&host, port, password_ref).await {
        Ok(_) => {
            info!("Connected to OBS successfully");

            // リプレイバッファ設定
            if let Err(e) = obs.set_replay_buffer().await {
                return Err(format!("Failed to set replay buffer: {}", e));
            }

            // VLCソース初期化
            if let Err(e) = obs.init_vlc_source().await {
                return Err(format!("Failed to init VLC source: {}", e));
            }

            // 接続情報を保存
            {
                let mut conn_info = state.obs_connection_info.lock().unwrap();
                *conn_info = Some((host.clone(), port, password.clone()));
            }

            // システム開始
            start_system(host, port, password, state, app_handle).await?;

            Ok("OBS接続に成功しました".to_string())
        }
        Err(e) => {
            error!("Failed to connect to OBS: {}", e);
            Err(format!("OBS接続に失敗しました: {}", e))
        }
    }
}

async fn start_system(
    host: String,
    port: u16,
    password: Option<String>,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    info!("Starting RL Replay system...");

    // システム動作中のフラグを設定
    {
        let mut is_running = state.is_system_running.lock().unwrap();
        *is_running = true;
    }

    // 別タスクでメインシステムを起動
    let host_clone = host.clone();
    let password_clone = password.clone();
    let sleep_duration_clone = state.sleep_duration_sec.clone();
    tokio::spawn(async move {
        if let Err(e) = run_main_system(
            host_clone,
            port,
            password_clone,
            sleep_duration_clone,
            app_handle,
        )
        .await
        {
            error!("Main system error: {}", e);
        }
    });

    info!("RL Replay system started successfully");
    Ok(())
}

async fn run_main_system(
    host: String,
    port: u16,
    password: Option<String>,
    sleep_duration: Arc<RwLock<u64>>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // OBS接続を再作成
    let mut obs = obs::Obs::new();
    let password_ref = password.as_deref();
    obs.connect(&host, port, password_ref)
        .await
        .map_err(|e| format!("Failed to reconnect to OBS: {}", e))?;

    obs.set_replay_buffer()
        .await
        .map_err(|e| format!("Failed to set replay buffer: {}", e))?;

    obs.init_vlc_source()
        .await
        .map_err(|e| format!("Failed to init VLC source: {}", e))?;

    // VlcManager初期化
    let vlc_manager = VlcManager::new();

    // イベントリスナー設定
    let (rb_tx, rb_rx) = mpsc::channel(32);
    obs.set_event_listener(rb_tx)
        .await
        .map_err(|e| format!("Failed to set event listener: {}", e))?;

    vlc_manager.set_event_listener(rb_rx, app_handle.clone());

    // UDPサーバー開始
    let (tx, mut rx) = mpsc::channel::<String>(32);
    tokio::spawn(async {
        if let Err(e) = bind_socket(tx).await {
            error!("UDP socket error: {}", e);
        }
    });

    // UDPメッセージ処理 - 無限ループで動作し続ける
    while let Some(d) = rx.recv().await {
        let cmd = mugi_schema::parse_cmd(&d);
        match cmd {
            Err(_) => error!("Failed to parse:{}", d),
            Ok(cmd) => {
                if cmd == MugiCmd::Scored || cmd == MugiCmd::EpicSave {
                    debug!("OBS fire!");
                    let duration = {
                        let sleep_dur = sleep_duration.read().unwrap();
                        *sleep_dur
                    };
                    tokio::time::sleep(std::time::Duration::from_secs(duration)).await;
                    if let Err(e) = obs.save_replay_buffer().await {
                        error!("Failed to save replay buffer: {}", e);
                    }
                }
            }
        }
    }

    info!("UDP receiver closed, system shutting down");
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    console_subscriber::init();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .target(Target::new(TargetKind::Folder {
                    path: std::path::PathBuf::from("./logs"),
                    file_name: None,
                }))
                .level(log::LevelFilter::Debug)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                update(handle).await.unwrap();
            });
            Ok(())
        })
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            connect_obs,
            play_highlights,
            set_sleep_duration,
            get_sleep_duration
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn update(app: AppHandle) -> tauri_plugin_updater::Result<()> {
    if let Some(update) = app.updater()?.check().await? {
        let mut downloaded = 0;
        update
            .download_and_install(
                |chunk_length, content_length| {
                    downloaded += chunk_length;
                    info!("downloaded {downloaded} from {content_length:?}");
                },
                || {
                    info!("download finished");
                },
            )
            .await?;
        info!("update installed");
        app.restart();
    }
    Ok(())
}
