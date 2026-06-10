//! Vox World launcher: checks the release manifest, downloads/updates the
//! game client, and launches it pointed at the official server.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use serde::Deserialize;
use sha2::Digest;

/// Stable URL: always points at the assets of the latest GitHub release.
const MANIFEST_URL: &str =
    "https://github.com/dogjung86-cloud/VoxWorld/releases/latest/download/manifest.json";
const HOMEPAGE_FALLBACK: &str = "https://dogjung86-cloud.github.io/VoxWorld/";

#[derive(Deserialize, Clone, Default)]
struct Manifest {
    version: String,
    windows_zip_url: String,
    #[serde(default)]
    windows_zip_sha256: Option<String>,
    #[serde(default)]
    windows_zip_size: Option<u64>,
    server_address: String,
    #[serde(default)]
    web_play_url: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Clone, PartialEq)]
enum Phase {
    Checking,
    ReadyPlay,
    NeedInstall,
    NeedUpdate,
    Downloading,
    Extracting,
    Offline,
    Error(String),
}

struct Shared {
    phase: Phase,
    manifest: Option<Manifest>,
    installed: Option<String>,
    downloaded: u64,
    total: u64,
}

struct Paths {
    root: PathBuf,
}

impl Paths {
    fn new() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: base.join("VoxWorld"),
        }
    }

    fn client(&self) -> PathBuf {
        self.root.join("client")
    }

    fn client_new(&self) -> PathBuf {
        self.root.join("client_new")
    }

    fn client_old(&self) -> PathBuf {
        self.root.join("client_old")
    }

    fn userdata(&self) -> PathBuf {
        self.root.join("userdata")
    }

    fn version_file(&self) -> PathBuf {
        self.root.join("installed_version.txt")
    }

    fn tmp_zip(&self) -> PathBuf {
        self.root.join("update.zip")
    }

    fn exe(&self) -> PathBuf {
        self.client().join("voxworld-client.exe")
    }
}

fn installed_version(paths: &Paths) -> Option<String> {
    let v = std::fs::read_to_string(paths.version_file()).ok()?;
    let v = v.trim().to_string();
    if v.is_empty() || !paths.exe().exists() {
        None
    } else {
        Some(v)
    }
}

fn check_thread(shared: Arc<Mutex<Shared>>, ctx: eframe::egui::Context) {
    thread::spawn(move || {
        let paths = Paths::new();
        let installed = installed_version(&paths);
        {
            let mut s = shared.lock().unwrap();
            s.installed = installed.clone();
        }

        let result = ureq::get(MANIFEST_URL)
            .timeout(Duration::from_secs(15))
            .call()
            .map_err(|e| e.to_string())
            .and_then(|r| r.into_string().map_err(|e| e.to_string()))
            .and_then(|body| {
                // Tolerate a UTF-8 BOM in case the manifest was written by a
                // Windows tool; serde_json rejects it otherwise.
                let body = body.trim_start_matches('\u{feff}');
                serde_json::from_str::<Manifest>(body).map_err(|e| e.to_string())
            });

        let mut s = shared.lock().unwrap();
        match result {
            Ok(m) => {
                s.phase = match &s.installed {
                    Some(v) if *v == m.version => Phase::ReadyPlay,
                    Some(_) => Phase::NeedUpdate,
                    None => Phase::NeedInstall,
                };
                s.manifest = Some(m);
            },
            Err(_) => {
                // Could not reach the update server; allow playing an
                // existing install.
                s.phase = if s.installed.is_some() {
                    Phase::Offline
                } else {
                    Phase::Error(
                        "업데이트 서버에 연결할 수 없습니다. 인터넷 연결을 확인해 주세요."
                            .to_string(),
                    )
                };
            },
        }
        ctx.request_repaint();
    });
}

fn download_thread(shared: Arc<Mutex<Shared>>, ctx: eframe::egui::Context) {
    thread::spawn(move || {
        let paths = Paths::new();
        let manifest = match shared.lock().unwrap().manifest.clone() {
            Some(m) => m,
            None => return,
        };

        let fail = |shared: &Arc<Mutex<Shared>>, ctx: &eframe::egui::Context, msg: String| {
            shared.lock().unwrap().phase = Phase::Error(msg);
            ctx.request_repaint();
        };

        if let Err(e) = std::fs::create_dir_all(&paths.root) {
            return fail(&shared, &ctx, format!("폴더 생성 실패: {e}"));
        }

        // Download with progress.
        let resp = match ureq::get(&manifest.windows_zip_url)
            .timeout(Duration::from_secs(60 * 60))
            .call()
        {
            Ok(r) => r,
            Err(e) => return fail(&shared, &ctx, format!("다운로드 실패: {e}")),
        };
        let total = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .or(manifest.windows_zip_size)
            .unwrap_or(0);
        shared.lock().unwrap().total = total;

        let mut reader = resp.into_reader();
        let mut file = match std::fs::File::create(paths.tmp_zip()) {
            Ok(f) => f,
            Err(e) => return fail(&shared, &ctx, format!("임시 파일 생성 실패: {e}")),
        };
        let mut buf = [0u8; 128 * 1024];
        let mut done: u64 = 0;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = file.write_all(&buf[..n]) {
                        return fail(&shared, &ctx, format!("디스크 쓰기 실패: {e}"));
                    }
                    done += n as u64;
                    let mut s = shared.lock().unwrap();
                    s.downloaded = done;
                    drop(s);
                    ctx.request_repaint_after(Duration::from_millis(100));
                },
                Err(e) => return fail(&shared, &ctx, format!("다운로드 중단: {e}")),
            }
        }
        drop(file);

        // Verify integrity when the manifest provides a checksum.
        if let Some(expect) = manifest
            .windows_zip_sha256
            .as_ref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
        {
            let mut hasher = sha2::Sha256::new();
            let mut f = match std::fs::File::open(paths.tmp_zip()) {
                Ok(f) => f,
                Err(e) => return fail(&shared, &ctx, format!("검증 실패: {e}")),
            };
            let mut buf = [0u8; 1024 * 1024];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => hasher.update(&buf[..n]),
                    Err(e) => return fail(&shared, &ctx, format!("검증 실패: {e}")),
                }
            }
            let got = format!("{:x}", hasher.finalize());
            if got != expect {
                let _ = std::fs::remove_file(paths.tmp_zip());
                return fail(
                    &shared,
                    &ctx,
                    "다운로드한 파일이 손상되었습니다. 다시 시도해 주세요.".to_string(),
                );
            }
        }

        shared.lock().unwrap().phase = Phase::Extracting;
        ctx.request_repaint();

        // Extract into a fresh directory, then swap it in.
        let _ = std::fs::remove_dir_all(paths.client_new());
        let file = match std::fs::File::open(paths.tmp_zip()) {
            Ok(f) => f,
            Err(e) => return fail(&shared, &ctx, format!("압축 해제 실패: {e}")),
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(e) => return fail(&shared, &ctx, format!("압축 파일이 올바르지 않습니다: {e}")),
        };
        if let Err(e) = archive.extract(paths.client_new()) {
            return fail(&shared, &ctx, format!("압축 해제 실패: {e}"));
        }

        // The zip may contain the payload at its root or inside a single
        // top-level folder; normalize to client_new/voxworld-client.exe.
        let mut payload = paths.client_new();
        if !payload.join("voxworld-client.exe").exists() {
            if let Ok(mut entries) = std::fs::read_dir(&payload) {
                if let Some(Ok(only)) = entries.next() {
                    if only.path().join("voxworld-client.exe").exists() {
                        payload = only.path();
                    }
                }
            }
        }
        if !payload.join("voxworld-client.exe").exists() {
            return fail(
                &shared,
                &ctx,
                "압축 파일 안에서 게임 실행 파일을 찾지 못했습니다.".to_string(),
            );
        }

        let _ = std::fs::remove_dir_all(paths.client_old());
        if paths.client().exists() {
            if let Err(e) = std::fs::rename(paths.client(), paths.client_old()) {
                return fail(
                    &shared,
                    &ctx,
                    format!("기존 설치 교체 실패 (게임이 실행 중인지 확인): {e}"),
                );
            }
        }
        if let Err(e) = std::fs::rename(&payload, paths.client()) {
            // Try to roll back so the user keeps a working install.
            let _ = std::fs::rename(paths.client_old(), paths.client());
            return fail(&shared, &ctx, format!("설치 실패: {e}"));
        }
        let _ = std::fs::remove_dir_all(paths.client_old());
        let _ = std::fs::remove_dir_all(paths.client_new());
        let _ = std::fs::remove_file(paths.tmp_zip());
        if let Err(e) = std::fs::write(paths.version_file(), &manifest.version) {
            return fail(&shared, &ctx, format!("버전 기록 실패: {e}"));
        }

        let mut s = shared.lock().unwrap();
        s.installed = Some(manifest.version.clone());
        s.phase = Phase::ReadyPlay;
        drop(s);
        ctx.request_repaint();
    });
}

fn launch_game(shared: &Arc<Mutex<Shared>>) -> Result<(), String> {
    let paths = Paths::new();
    let server = shared
        .lock()
        .unwrap()
        .manifest
        .as_ref()
        .map(|m| m.server_address.clone());
    let _ = std::fs::create_dir_all(paths.userdata());

    let mut cmd = std::process::Command::new(paths.exe());
    cmd.current_dir(paths.client())
        .env("VOXWORLD_USERDATA", paths.userdata());
    if let Some(server) = server.filter(|s| !s.is_empty()) {
        cmd.arg("--server").arg(server);
    }
    cmd.spawn().map_err(|e| format!("게임 실행 실패: {e}"))?;
    Ok(())
}

struct App {
    shared: Arc<Mutex<Shared>>,
    korean: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Load a Korean-capable system font; fall back to English labels if
        // unavailable (egui's bundled fonts have no Hangul glyphs).
        let mut korean = false;
        for candidate in [
            "C:\\Windows\\Fonts\\malgun.ttf",
            "C:\\Windows\\Fonts\\malgunsl.ttf",
            "C:\\Windows\\Fonts\\NanumGothic.ttf",
        ] {
            if let Ok(bytes) = std::fs::read(candidate) {
                let mut fonts = eframe::egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "korean".to_owned(),
                    eframe::egui::FontData::from_owned(bytes).into(),
                );
                for family in [
                    eframe::egui::FontFamily::Proportional,
                    eframe::egui::FontFamily::Monospace,
                ] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .insert(0, "korean".to_owned());
                }
                cc.egui_ctx.set_fonts(fonts);
                korean = true;
                break;
            }
        }

        let shared = Arc::new(Mutex::new(Shared {
            phase: Phase::Checking,
            manifest: None,
            installed: None,
            downloaded: 0,
            total: 0,
        }));
        check_thread(shared.clone(), cc.egui_ctx.clone());
        Self { shared, korean }
    }

    fn t<'a>(&self, ko: &'a str, en: &'a str) -> &'a str {
        if self.korean { ko } else { en }
    }
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;

        let (phase, manifest, installed, downloaded, total) = {
            let s = self.shared.lock().unwrap();
            (
                s.phase.clone(),
                s.manifest.clone(),
                s.installed.clone(),
                s.downloaded,
                s.total,
            )
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(18.0);
            ui.vertical_centered(|ui| {
                ui.heading(
                    egui::RichText::new("VOX WORLD")
                        .size(34.0)
                        .strong()
                        .color(egui::Color32::from_rgb(120, 200, 255)),
                );
                ui.label(
                    egui::RichText::new(self.t(
                        "복셀 판타지 멀티플레이어 RPG",
                        "A multiplayer voxel fantasy RPG",
                    ))
                    .size(13.0)
                    .weak(),
                );
            });
            ui.add_space(14.0);
            ui.separator();
            ui.add_space(10.0);

            match &manifest {
                Some(m) => {
                    ui.label(format!(
                        "{}: {}",
                        self.t("최신 버전", "Latest version"),
                        m.version
                    ));
                    ui.label(format!(
                        "{}: {}",
                        self.t("설치된 버전", "Installed version"),
                        installed.as_deref().unwrap_or(self.t("없음", "none"))
                    ));
                    ui.label(format!(
                        "{}: {}",
                        self.t("게임 서버", "Game server"),
                        m.server_address
                    ));
                },
                None => {
                    ui.label(format!(
                        "{}: {}",
                        self.t("설치된 버전", "Installed version"),
                        installed.as_deref().unwrap_or(self.t("없음", "none"))
                    ));
                },
            }

            ui.add_space(12.0);

            match &phase {
                Phase::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(self.t("업데이트 확인 중...", "Checking for updates..."));
                    });
                },
                Phase::Downloading => {
                    let frac = if total > 0 {
                        downloaded as f32 / total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    ui.label(format!("{} / {}", fmt_mb(downloaded), fmt_mb(total)));
                    ctx.request_repaint_after(Duration::from_millis(150));
                },
                Phase::Extracting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(self.t("압축 푸는 중...", "Extracting..."));
                    });
                    ctx.request_repaint_after(Duration::from_millis(150));
                },
                Phase::Error(msg) => {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), msg);
                },
                Phase::Offline => {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 120),
                        self.t(
                            "업데이트 확인 실패 — 설치된 버전으로 플레이할 수 있습니다.",
                            "Update check failed — you can still play the installed version.",
                        ),
                    );
                },
                _ => {},
            }

            ui.add_space(14.0);

            ui.vertical_centered(|ui| {
                let button = |text: &str| {
                    egui::Button::new(egui::RichText::new(text).size(20.0).strong())
                        .min_size(egui::vec2(260.0, 48.0))
                };
                match phase {
                    Phase::ReadyPlay | Phase::Offline => {
                        if ui.add(button(self.t("게임 시작", "PLAY"))).clicked() {
                            match launch_game(&self.shared) {
                                Ok(()) => {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                },
                                Err(e) => {
                                    self.shared.lock().unwrap().phase = Phase::Error(e);
                                },
                            }
                        }
                    },
                    Phase::NeedInstall => {
                        if ui.add(button(self.t("설치하기", "INSTALL"))).clicked() {
                            self.shared.lock().unwrap().phase = Phase::Downloading;
                            download_thread(self.shared.clone(), ctx.clone());
                        }
                    },
                    Phase::NeedUpdate => {
                        if ui.add(button(self.t("업데이트", "UPDATE"))).clicked() {
                            self.shared.lock().unwrap().phase = Phase::Downloading;
                            download_thread(self.shared.clone(), ctx.clone());
                        }
                    },
                    Phase::Error(_) => {
                        if ui
                            .add(button(self.t("다시 시도", "RETRY")))
                            .clicked()
                        {
                            self.shared.lock().unwrap().phase = Phase::Checking;
                            check_thread(self.shared.clone(), ctx.clone());
                        }
                    },
                    _ => {
                        ui.add_enabled(false, button(self.t("잠시만요...", "PLEASE WAIT...")));
                    },
                }
            });

            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                let homepage = manifest
                    .as_ref()
                    .and_then(|m| m.homepage.clone())
                    .unwrap_or_else(|| HOMEPAGE_FALLBACK.to_string());
                ui.hyperlink_to(self.t("공식 홈페이지", "Official website"), homepage);
                if let Some(web) = manifest.as_ref().and_then(|m| m.web_play_url.clone()) {
                    ui.hyperlink_to(
                        self.t("브라우저에서 바로 플레이", "Play in browser"),
                        web,
                    );
                }
                if let Some(notes) = manifest.as_ref().and_then(|m| m.notes.clone()) {
                    if !notes.is_empty() {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(notes).size(12.0).weak());
                    }
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([560.0, 460.0])
            .with_min_inner_size([480.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Vox World Launcher",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
